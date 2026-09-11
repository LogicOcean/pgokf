// SPDX-License-Identifier: AGPL-3.0-only
//! Generation-bound typed relationships (capability C): the catalog-owned,
//! producer-neutral relationship projection, the controlled
//! `replace_relationships` write API, and the typed traversal
//! `concept_relationship_neighbors`.
//!
//! This module is deliberately separate from [`crate::catalog::links`]:
//! authored Markdown links are a same-bundle authoring convenience projected by
//! the sync engine, while typed relationships are producer-published,
//! cross-bundle, and bound to the catalog generation they were computed
//! against. The catalog stores producer-defined relation types as opaque
//! namespaced text and never enumerates, infers, or interprets them.
//!
//! # Producer identity
//!
//! As everywhere in the producer-facing surface, `producer` is a
//! **caller-supplied opaque label, not authorization**; authorization is
//! `session_user` membership in the writer tier (see [`crate::security`]).
//!
//! # Publication model
//!
//! `pgokf.relationship_publication` is the immutable attempt/result record of
//! one producer's relationship set for one source bundle, keyed
//! `(tenant_id, producer, source_bundle_id, publication_generation)` and bound
//! to a live [`pgokf.publication_fence`](crate::catalog::freshness) slot (the
//! `fencing_token` must be the slot's live, unexpired token and
//! `publication_generation` must equal the fence's target generation, so a
//! superseded or expired attempt can never publish). The publication records
//! the expected catalog generation it was computed against, the BLAKE3
//! relationship-set hash (doubling as the idempotency key), its state
//! (`staged|active|superseded`), and its timestamps. `pgokf.relationship`
//! carries the rows of one publication; every graph key is
//! `(bundle_id, concept_id)` because concept ids are unique only within a
//! bundle.
//!
//! # Generation semantics (exact)
//!
//! `replace_relationships(producer, source_bundle, publication_generation,
//! expected_catalog_generation, fencing_token, rows)` runs compare-and-set
//! under the source bundle's advisory lock and admits exactly two generation
//! positions, evaluated against the bundle's **current** catalog generation
//! `G`:
//!
//! - `expected = G` (the rows describe the committed current concepts): the
//!   publication is **active immediately** - it supersedes the producer's
//!   prior active publication for the bundle in the same transaction. Source
//!   and resolved-target concept endpoints are validated against the current
//!   catalog (an absent source concept is `22023`).
//! - `expected = G + 1` (the rows describe a refresh that has not run yet):
//!   the publication is **staged** - invisible to every reader - until
//!   [`crate::catalog::sync::run_bundle_sync`] accepts exactly that
//!   generation, at which point the sync transaction itself activates it.
//!   Concept endpoints cannot be validated against a future concept set, so
//!   endpoint resolution is deferred: activation re-resolves every declared
//!   target concept against the accepted concepts (a still-absent target stays
//!   `unresolved`) and quarantines every row whose source concept the accepted
//!   concepts do not contain (the row is deleted, so a nonexistent source can
//!   never surface as a graph node or in `current_relationships`).
//! - anything else (stale or further-ahead `expected`) is rejected with
//!   `22023`. A write computed against an old catalog generation never
//!   becomes visible.
//!
//! Inside a refresh, activation happens **in the sync transaction after the
//! new catalog generation is established** ([`activate_staged`]): every active
//! publication of the bundle is generation-bound to the pre-refresh concepts
//! and is superseded, staged publications expecting the accepted generation
//! activate, and staged publications expecting an older generation (their
//! refresh raced an intervening state mutation, which also bumps the
//! generation) are superseded. Commit ordering therefore guarantees that no
//! query can ever combine new concept content with old-generation
//! relationships.
//!
//! **One winner per producer scope.** When competing staged attempts of the
//! same `(tenant_id, producer, source_bundle_id)` scope expect the accepted
//! generation (a producer staged a replacement, then re-fenced and staged
//! again), only the newest attempt - the highest `publication_generation` -
//! activates; the rest are superseded. Exactly one publication per producer
//! scope activates per generation, so an empty winning set really replaces the
//! prior set.
//!
//! **Bounded superseded retention.** Superseded publications are audit, but
//! not unbounded: every activation (immediate or refresh-time) hard-deletes
//! superseded publications whose supersession (`updated_at`) is more than 30
//! days old - the activating bundle's own and the detached ledger's
//! (`source_bundle_id IS NULL` after an unregister/purge) alike - their rows
//! cascading with them (the acknowledged change-event outbox precedent), and
//! every unregister/purge sweeps the aged detached rows it leaves behind, so
//! detached history never outlives the retention window. The immediately
//! previous superseded set is by construction younger than the window, so its
//! rows and activation evidence always survive.
//!
//! **Required coverage.** A bundle that had relationship coverage (at least
//! one active publication) before a refresh and activates none at the new
//! generation is marked `stale` with the catalog-defined reason
//! `relationship_coverage_missing` in the same transaction, and the
//! compare-and-set [`pgokf.mark_fresh`](crate::catalog::freshness) refuses
//! (returns `false`) while that reason stands - the producer must publish a
//! matching replacement (which clears the reason) before its reconciliation
//! can complete. A bundle that never published relationships is unaffected.
//!
//! An **empty** `rows` array is a deliberate empty set: it supersedes the
//! prior set on activation. **Idempotent retry:** re-presenting the same
//! `(producer, source_bundle, publication_generation)` key with the same
//! canonical relationship set is a no-op returning the existing publication;
//! the same key with a different set is a conflict (SQLSTATE `23505`).
//!
//! # Endpoint validation and the no-leak rule
//!
//! A resolved target endpoint `(target_bundle_id, target_concept_id)` is
//! validated at write time against the **writer's** visibility: an active
//! (`enabled AND retired_at IS NULL`), tenant-visible bundle containing the
//! concept. An absent, inactive, or cross-tenant target yields the identical
//! outcome - the row is stored `unresolved` with the bundle reference dropped
//! (the producer-declared concept id is retained as opaque metadata) - so the
//! API can never confirm whether another tenant holds a bundle.
//!
//! # Reader surface and traversal
//!
//! Readers never see the raw tables (no grant; the standard tenant RLS is
//! defense in depth). `pgokf.current_relationships` exposes only `active`
//! publications of active source bundles, applies the standard opt-in tenant
//! predicate inline, and hides a resolved row whose target bundle is not
//! currently active and tenant-visible (unresolved rows are returned as
//! metadata, never materialized as nonexistent references). Bundle retirement
//! therefore removes current relationship visibility without touching the
//! retained audit rows, and unretirement restores it.
//!
//! `pgokf.concept_relationship_neighbors` is the typed counterpart of
//! [`crate::catalog::neighbors`]: the same cycle-safe level BFS, but keyed on
//! `(bundle_id, concept_id)`, over `current_relationships`, with
//! `outbound|inbound|both` direction, optional relation-type filters, and a
//! per-node freshness + embedding-provenance annotation from
//! `pgokf.effective_freshness` (concept > path > bundle precedence), matching
//! the search surfacing contract. Unresolved/external rows never become
//! traversal edges. `pgokf.concept_neighbors` is unchanged.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use okf_sync::hash_bytes;
use pgrx::datum::TimestampWithTimeZone;
use pgrx::heap_tuple::PgHeapTuple;
use pgrx::spi::SpiHeapTupleData;
use pgrx::{AllocatedByRust, JsonB, Spi, extension_sql};

use crate::catalog::batch::BATCH_SIZE;
use crate::catalog::spi_read::RowReader;
use crate::catalog::sync::advisory_lock_key;
use crate::errors::CatalogError;
use crate::security;

/// Maximum rows accepted in one `replace_relationships` call (a hard limit;
/// inserts are batched at [`BATCH_SIZE`] beneath it).
const MAX_RELATIONSHIP_ROWS: usize = 10_000;
/// Maximum length of a producer-defined namespaced relation type.
const RELATION_TYPE_MAX_LEN: usize = 128;
/// Maximum length of a concept id endpoint.
const CONCEPT_ID_MAX_LEN: usize = 1024;
/// Maximum length of an opaque external target identifier.
const EXTERNAL_TARGET_MAX_LEN: usize = 1024;
/// Maximum serialized size of one opaque jsonb row field
/// (`source_location` / `provenance`).
const OPAQUE_FIELD_MAX_BYTES: usize = 16_384;
/// Hard ceiling for the traversal's `max_results` (the default is 500).
const MAX_NEIGHBOR_RESULTS: i32 = 10_000;
/// Maximum number of relation-type filters the traversal accepts.
const MAX_RELATION_TYPE_FILTERS: usize = 256;

/// The direction values a relationship row may carry: `directed` (the
/// default; traversed source -> target) or `undirected` (traversed both ways).
const DIRECTIONS: [&str; 2] = ["directed", "undirected"];

/// The direction modes `concept_relationship_neighbors` accepts.
const TRAVERSAL_DIRECTIONS: [&str; 3] = ["outbound", "inbound", "both"];

fn spi_error(context: &str, error: &pgrx::spi::Error) -> CatalogError {
    CatalogError::internal(format!("{context}: {error}"), Path::new(""))
}

/// The shared `22023` shape for an unregistered (or cross-tenant) bundle id.
fn unknown_bundle_error(bundle_id: i64) -> CatalogError {
    CatalogError::invalid_parameter(
        format!("bundle {bundle_id} is not registered"),
        Path::new(""),
    )
}

// ---------------------------------------------------------------------------
// Row wire shape: parsing, validation, canonicalization, hashing (pure Rust).
// ---------------------------------------------------------------------------

/// One validated relationship row, before endpoint resolution and hashing.
///
/// `source_location` and `provenance` are stored as their deterministic
/// serialized jsonb text (the input arrived as `jsonb`, so the in-memory value
/// already carries the database's normalization); the catalog never interprets
/// them.
#[derive(Debug, Clone, PartialEq)]
struct RelationshipRow {
    source_concept_id: String,
    relation_type: String,
    direction: String,
    /// Declared resolved-target bundle; `Some` iff `target_concept_id` is set.
    target_bundle_id: Option<i64>,
    target_concept_id: Option<String>,
    external_target: Option<String>,
    source_location: Option<String>,
    confidence: Option<f64>,
    provenance: Option<String>,
    /// Set by endpoint resolution: no live resolved target backs this row.
    unresolved: bool,
    /// Set by endpoint resolution: the declared target names another bundle.
    cross_bundle: bool,
}

/// The keys a row object may carry; anything else is rejected so a newer
/// producer writing to an older catalog fails loudly instead of silently
/// dropping fields.
const ROW_KEYS: [&str; 9] = [
    "source_concept_id",
    "relation_type",
    "direction",
    "target_bundle_id",
    "target_concept_id",
    "external_target",
    "source_location",
    "confidence",
    "provenance",
];

/// A row-shape violation, phrased with the row's position in the submission.
fn row_error(index: usize, message: impl std::fmt::Display) -> CatalogError {
    CatalogError::invalid_parameter(
        format!("relationship row at index {index}: {message}"),
        Path::new(""),
    )
}

/// Read a required nonempty, length-bounded string field of a row object.
fn required_text(
    index: usize,
    object: &serde_json::Map<String, serde_json::Value>,
    key: &str,
    max_len: usize,
) -> Result<String, CatalogError> {
    let value = object
        .get(key)
        .ok_or_else(|| row_error(index, format!("{key} is required")))?;
    let text = value
        .as_str()
        .ok_or_else(|| row_error(index, format!("{key} must be a string")))?;
    if text.is_empty() {
        return Err(row_error(index, format!("{key} must not be empty")));
    }
    if text.len() > max_len {
        return Err(row_error(
            index,
            format!("{key} exceeds the {max_len}-character limit"),
        ));
    }
    Ok(text.to_owned())
}

/// Read an optional nullable string field of a row object.
fn optional_text(
    index: usize,
    object: &serde_json::Map<String, serde_json::Value>,
    key: &str,
    max_len: usize,
) -> Result<Option<String>, CatalogError> {
    let Some(value) = object.get(key) else {
        return Ok(None);
    };
    if value.is_null() {
        return Ok(None);
    }
    let text = value
        .as_str()
        .ok_or_else(|| row_error(index, format!("{key} must be a string or null")))?;
    if text.is_empty() {
        return Err(row_error(index, format!("{key} must not be empty")));
    }
    if text.len() > max_len {
        return Err(row_error(
            index,
            format!("{key} exceeds the {max_len}-character limit"),
        ));
    }
    Ok(Some(text.to_owned()))
}

/// Validate a relation type: nonempty, length-bounded, and namespaced (it must
/// contain `:` separating the producer namespace from the relation name).
/// The catalog never enumerates or interprets the vocabulary.
fn validate_relation_type(index: usize, relation_type: &str) -> Result<(), CatalogError> {
    let namespaced = relation_type
        .split_once(':')
        .is_some_and(|(namespace, name)| !namespace.is_empty() && !name.is_empty());
    if !namespaced {
        return Err(row_error(
            index,
            format!(
                "relation_type must be namespaced as <namespace>:<name>, got {relation_type:?}"
            ),
        ));
    }
    Ok(())
}

/// Serialize an opaque jsonb field, bounding its size.
fn opaque_field(
    index: usize,
    object: &serde_json::Map<String, serde_json::Value>,
    key: &str,
) -> Result<Option<String>, CatalogError> {
    let Some(value) = object.get(key) else {
        return Ok(None);
    };
    if value.is_null() {
        return Ok(None);
    }
    let text = value.to_string();
    if text.len() > OPAQUE_FIELD_MAX_BYTES {
        return Err(row_error(
            index,
            format!("{key} exceeds the {OPAQUE_FIELD_MAX_BYTES}-byte limit"),
        ));
    }
    Ok(Some(text))
}

/// Parse and validate one row object of the `rows` wire array.
fn parse_row(index: usize, value: &serde_json::Value) -> Result<RelationshipRow, CatalogError> {
    let object = value
        .as_object()
        .ok_or_else(|| row_error(index, "each row must be a JSON object"))?;
    for key in object.keys() {
        if !ROW_KEYS.contains(&key.as_str()) {
            return Err(row_error(
                index,
                format!("unsupported key {key:?}; the v1 row shape allows only {ROW_KEYS:?}"),
            ));
        }
    }

    let source_concept_id = required_text(index, object, "source_concept_id", CONCEPT_ID_MAX_LEN)?;
    let relation_type = required_text(index, object, "relation_type", RELATION_TYPE_MAX_LEN)?;
    validate_relation_type(index, &relation_type)?;
    let direction =
        optional_text(index, object, "direction", 16)?.unwrap_or_else(|| "directed".to_owned());
    if !DIRECTIONS.contains(&direction.as_str()) {
        return Err(row_error(
            index,
            format!(
                "direction must be one of {}, got {direction:?}",
                DIRECTIONS.map(|d| format!("'{d}'")).join(", ")
            ),
        ));
    }

    let target_bundle_id = match object.get("target_bundle_id") {
        None | Some(serde_json::Value::Null) => None,
        Some(value) => {
            let raw = value
                .as_i64()
                .ok_or_else(|| row_error(index, "target_bundle_id must be an integer"))?;
            if raw <= 0 {
                return Err(row_error(index, "target_bundle_id must be greater than 0"));
            }
            Some(raw)
        }
    };
    let target_concept_id = optional_text(index, object, "target_concept_id", CONCEPT_ID_MAX_LEN)?;
    let external_target = optional_text(index, object, "external_target", EXTERNAL_TARGET_MAX_LEN)?;

    // Exactly one target shape: a resolved concept endpoint (concept id, with
    // an optional bundle id defaulting to the source bundle), or an opaque
    // external identifier, or nothing (an unresolved row).
    if target_bundle_id.is_some() && target_concept_id.is_none() {
        return Err(row_error(
            index,
            "target_bundle_id requires target_concept_id",
        ));
    }
    if target_concept_id.is_some() && external_target.is_some() {
        return Err(row_error(
            index,
            "a row names either a resolved target concept or an external target, not both",
        ));
    }

    let confidence = match object.get("confidence") {
        None | Some(serde_json::Value::Null) => None,
        Some(value) => {
            let raw = value
                .as_f64()
                .ok_or_else(|| row_error(index, "confidence must be a number"))?;
            if !(0.0..=1.0).contains(&raw) {
                return Err(row_error(
                    index,
                    format!("confidence must be between 0 and 1, got {raw}"),
                ));
            }
            Some(raw)
        }
    };

    Ok(RelationshipRow {
        source_concept_id,
        relation_type,
        direction,
        target_bundle_id,
        target_concept_id,
        external_target,
        source_location: opaque_field(index, object, "source_location")?,
        confidence,
        provenance: opaque_field(index, object, "provenance")?,
        unresolved: false,
        cross_bundle: false,
    })
}

/// Parse the `rows` wire argument: a JSON array of at most
/// [`MAX_RELATIONSHIP_ROWS`] validated row objects.
fn parse_rows(rows: &JsonB) -> Result<Vec<RelationshipRow>, CatalogError> {
    let array = rows.0.as_array().ok_or_else(|| {
        CatalogError::invalid_parameter("rows must be a JSON array of row objects", Path::new(""))
    })?;
    if array.len() > MAX_RELATIONSHIP_ROWS {
        return Err(CatalogError::invalid_parameter(
            format!(
                "rows holds {} entries but the hard limit is {MAX_RELATIONSHIP_ROWS}",
                array.len()
            ),
            Path::new(""),
        ));
    }
    array
        .iter()
        .enumerate()
        .map(|(index, value)| parse_row(index, value))
        .collect()
}

/// The canonical identity of a row for duplicate detection and ordering.
type CanonicalIdentity<'a> = (
    &'a str,
    &'a str,
    &'a str,
    Option<i64>,
    Option<&'a str>,
    Option<&'a str>,
);

fn canonical_identity(row: &RelationshipRow) -> CanonicalIdentity<'_> {
    (
        row.source_concept_id.as_str(),
        row.relation_type.as_str(),
        row.direction.as_str(),
        row.target_bundle_id,
        row.target_concept_id.as_deref(),
        row.external_target.as_deref(),
    )
}

/// Reject duplicate canonical identities within one submission (`22023`).
fn reject_duplicates(rows: &[RelationshipRow]) -> Result<(), CatalogError> {
    let mut seen: HashSet<CanonicalIdentity<'_>> = HashSet::with_capacity(rows.len());
    for row in rows {
        if !seen.insert(canonical_identity(row)) {
            return Err(CatalogError::invalid_parameter(
                format!(
                    "duplicate canonical identity in rows: source {} relation {}",
                    row.source_concept_id, row.relation_type
                ),
                Path::new(""),
            ));
        }
    }
    Ok(())
}

/// The canonical text one row hashes to: its identity fields plus confidence
/// and the opaque jsonb fields, `\0`-separated (field boundaries are
/// unambiguous because the separator cannot be confused across positions).
fn canonical_row_text(row: &RelationshipRow) -> String {
    let mut text = String::new();
    text.push_str(&row.source_concept_id);
    text.push('\0');
    text.push_str(&row.relation_type);
    text.push('\0');
    text.push_str(&row.direction);
    text.push('\0');
    text.push_str(
        &row.target_bundle_id
            .map_or_else(|| "-".into(), |id| id.to_string()),
    );
    text.push('\0');
    text.push_str(row.target_concept_id.as_deref().unwrap_or("-"));
    text.push('\0');
    text.push_str(row.external_target.as_deref().unwrap_or("-"));
    text.push('\0');
    text.push_str(&row.confidence.map_or_else(|| "-".into(), |c| c.to_string()));
    text.push('\0');
    text.push_str(row.source_location.as_deref().unwrap_or("-"));
    text.push('\0');
    text.push_str(row.provenance.as_deref().unwrap_or("-"));
    text
}

/// Canonicalize the set: sort rows by canonical identity and compute each
/// row's BLAKE3 hash plus the set hash (the BLAKE3 digest of the row hashes in
/// canonical order). The set hash is the publication's idempotency key:
/// identical retried input hashes identically regardless of submission order.
fn canonicalize(mut rows: Vec<RelationshipRow>) -> (Vec<(RelationshipRow, String)>, String) {
    rows.sort_by(|left, right| canonical_identity(left).cmp(&canonical_identity(right)));
    let hashed: Vec<(RelationshipRow, String)> = rows
        .into_iter()
        .map(|row| {
            let row_hash = hash_bytes(canonical_row_text(&row).as_bytes());
            (row, row_hash)
        })
        .collect();
    let mut combined = String::new();
    for (_, row_hash) in &hashed {
        combined.push_str(row_hash);
        combined.push('\n');
    }
    (hashed, hash_bytes(combined.as_bytes()))
}

// ---------------------------------------------------------------------------
// Endpoint resolution (no-leak rule).
// ---------------------------------------------------------------------------

/// The active, tenant-visible bundle ids out of `candidates` (owner-rights
/// read: the caller is the `SECURITY DEFINER` body, tenant-confined
/// explicitly). An absent, inactive, or cross-tenant id is simply missing
/// from the result - indistinguishable outcomes by construction.
fn visible_active_bundles(candidates: &[i64]) -> Result<HashSet<i64>, CatalogError> {
    if candidates.is_empty() {
        return Ok(HashSet::new());
    }
    Spi::connect(|client| {
        let table = client
            .select(
                "SELECT b.id
                 FROM pgokf.bundles b
                 WHERE b.id = ANY ($1)
                   AND b.enabled AND b.retired_at IS NULL
                   AND (((pg_catalog.current_setting('pgokf.tenant', true) IS NULL
                         OR pg_catalog.current_setting('pgokf.tenant', true) = '')
                        AND NOT (SELECT pgokf.tenant_required()))
                     OR b.tenant_id = pg_catalog.current_setting('pgokf.tenant', true))",
                None,
                &[candidates.to_vec().into()],
            )
            .map_err(|error| spi_error("failed to resolve target bundle visibility", &error))?;
        let mut visible = HashSet::with_capacity(table.len());
        for row in table {
            visible.insert(
                RowReader::new(&row, "failed to read target bundle", "bundle")
                    .required::<i64>(1, "id")?,
            );
        }
        Ok(visible)
    })
}

/// The `(bundle_id, concept_id)` pairs out of `candidates` that exist.
fn existing_concept_pairs(
    candidates: &[(i64, &str)],
) -> Result<HashSet<(i64, String)>, CatalogError> {
    if candidates.is_empty() {
        return Ok(HashSet::new());
    }
    let bundle_ids: Vec<i64> = candidates.iter().map(|(bundle, _)| *bundle).collect();
    let concept_ids: Vec<&str> = candidates.iter().map(|(_, id)| *id).collect();
    Spi::connect(|client| {
        let table = client
            .select(
                "SELECT c.bundle_id, c.id
                 FROM pgokf.concepts c
                 JOIN (SELECT * FROM unnest($1::bigint[], $2::text[]))
                      AS t(bundle_id, concept_id)
                   ON c.bundle_id = t.bundle_id AND c.id = t.concept_id",
                None,
                &[bundle_ids.into(), concept_ids.into()],
            )
            .map_err(|error| spi_error("failed to resolve target concepts", &error))?;
        let mut existing = HashSet::with_capacity(table.len());
        for row in table {
            let reader = RowReader::new(&row, "failed to read target concept", "concept");
            existing.insert((
                reader.required::<i64>(1, "bundle_id")?,
                reader.required::<String>(2, "id")?,
            ));
        }
        Ok(existing)
    })
}

/// Resolve every row's endpoint flags against the current catalog.
///
/// In `validate_concepts` mode (an immediately activating write) the source
/// concept must exist in the source bundle (`22023` otherwise) and a declared
/// target concept in a visible bundle must exist to resolve. In staged mode
/// the concept set is a future refresh's, so concept existence is deferred to
/// activation; only bundle visibility is decided now. A declared target bundle
/// that is absent, inactive, or cross-tenant is dropped from the row (the
/// concept id stays as opaque metadata), so an invisible endpoint is
/// indistinguishable from an absent one.
fn resolve_endpoints(
    source_bundle_id: i64,
    rows: &mut [RelationshipRow],
    validate_concepts: bool,
) -> Result<(), CatalogError> {
    // A concept-only target names a concept in the SOURCE bundle (the
    // resolved-target bundle defaults), so every resolved-shape row carries
    // both halves of its endpoint.
    for row in rows.iter_mut() {
        if row.target_concept_id.is_some() && row.target_bundle_id.is_none() {
            row.target_bundle_id = Some(source_bundle_id);
        }
    }

    let declared_bundles: Vec<i64> = rows
        .iter()
        .filter_map(|row| row.target_bundle_id)
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();
    let visible = visible_active_bundles(&declared_bundles)?;

    for row in rows.iter_mut() {
        row.cross_bundle = row
            .target_bundle_id
            .is_some_and(|target| target != source_bundle_id);
        if let Some(target) = row.target_bundle_id
            && !visible.contains(&target)
        {
            // Invisible/absent/inactive target bundle: unresolved, and BOTH
            // endpoint references are dropped, so nothing about the invisible
            // endpoint is recorded (and the pair-shape CHECK holds).
            row.target_bundle_id = None;
            row.target_concept_id = None;
            row.unresolved = true;
        }
    }

    if validate_concepts {
        let source_ids: Vec<&str> = rows
            .iter()
            .map(|row| row.source_concept_id.as_str())
            .collect::<HashSet<_>>()
            .into_iter()
            .collect();
        let existing_sources: HashSet<String> = Spi::connect(|client| {
            let table = client
                .select(
                    "SELECT c.id FROM pgokf.concepts c
                     WHERE c.bundle_id = $1 AND c.id = ANY ($2)",
                    None,
                    &[source_bundle_id.into(), source_ids.into()],
                )
                .map_err(|error| spi_error("failed to resolve source concepts", &error))?;
            let mut ids = HashSet::with_capacity(table.len());
            for row in table {
                ids.insert(
                    RowReader::new(&row, "failed to read source concept", "concept")
                        .required::<String>(1, "id")?,
                );
            }
            Ok::<_, CatalogError>(ids)
        })?;
        for row in rows.iter() {
            if !existing_sources.contains(&row.source_concept_id) {
                return Err(CatalogError::invalid_parameter(
                    format!(
                        "source concept {} does not exist in bundle {source_bundle_id}",
                        row.source_concept_id
                    ),
                    Path::new(""),
                ));
            }
        }

        let target_pairs: Vec<(i64, &str)> = rows
            .iter()
            .filter_map(
                |row| match (row.target_bundle_id, row.target_concept_id.as_deref()) {
                    (Some(bundle), Some(concept)) => Some((bundle, concept)),
                    _ => None,
                },
            )
            .collect();
        let existing_targets = existing_concept_pairs(&target_pairs)?;
        for row in rows.iter_mut() {
            if let (Some(bundle), Some(concept)) = (row.target_bundle_id, &row.target_concept_id)
                && !existing_targets.contains(&(bundle, concept.clone()))
            {
                // The bundle is visible but the concept is absent: unresolved,
                // both references retained (nothing leaks - the bundle is
                // visible to this session).
                row.unresolved = true;
            }
        }
    } else {
        // Staged rows: a declared target concept in a visible bundle may or may
        // not exist after the refresh; activation re-resolves it. Provisionally
        // unresolved until then.
        for row in rows.iter_mut() {
            if row.target_concept_id.is_some() && row.target_bundle_id.is_some() {
                row.unresolved = true;
            }
        }
    }

    // External and target-less rows are unresolved by definition.
    for row in rows.iter_mut() {
        if row.target_concept_id.is_none() {
            row.unresolved = true;
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// DDL: tables, indexes, RLS, comments, grants (fresh-install block).
// ---------------------------------------------------------------------------

extension_sql!(
    r"
CREATE TABLE pgokf.relationship_publication (
    publication_id    bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    tenant_id         text NOT NULL DEFAULT 'default',
    producer          text NOT NULL,
    source_bundle_id  bigint,
    source_bundle_path text NOT NULL,
    publication_generation bigint NOT NULL,
    expected_catalog_generation bigint NOT NULL,
    activated_catalog_generation bigint,
    fencing_token     bigint NOT NULL,
    relationship_set_hash text NOT NULL,
    idempotency_key   text NOT NULL,
    manifest_hash     text,
    state             text NOT NULL DEFAULT 'staged',
    row_count         integer NOT NULL DEFAULT 0,
    created_by        text NOT NULL DEFAULT session_user,
    created_at        timestamptz NOT NULL DEFAULT now(),
    activated_at      timestamptz,
    updated_at        timestamptz NOT NULL DEFAULT now(),
    CONSTRAINT relationship_publication_bundle_fk
        FOREIGN KEY (source_bundle_id) REFERENCES pgokf.bundles (id) ON DELETE SET NULL,
    CONSTRAINT relationship_publication_state_chk
        CHECK (state IN ('staged', 'active', 'superseded')),
    CONSTRAINT relationship_publication_generation_chk
        CHECK (publication_generation > 0 AND fencing_token > 0
               AND expected_catalog_generation >= 0),
    CONSTRAINT relationship_publication_activation_chk
        CHECK (state <> 'active' OR activated_catalog_generation IS NOT NULL)
);

-- Duplicate prevention for live bundles: one publication per
-- (tenant_id, producer, source_bundle_id, publication_generation). The
-- partial index excludes detached audit rows (source_bundle_id IS NULL after
-- the ON DELETE SET NULL detach), so hard-deleting two bundles that each held
-- the same producer/generation key never collides in the retained ledger.
CREATE UNIQUE INDEX relationship_publication_uq
    ON pgokf.relationship_publication
    (tenant_id, producer, source_bundle_id, publication_generation)
    WHERE source_bundle_id IS NOT NULL;

-- The sync-time activation scan: staged/active publications of one bundle.
CREATE INDEX relationship_publication_bundle_state_idx
    ON pgokf.relationship_publication (source_bundle_id, state);

CREATE TABLE pgokf.relationship (
    publication_id    bigint NOT NULL,
    ordinal           integer NOT NULL,
    tenant_id         text NOT NULL DEFAULT 'default',
    source_bundle_id  bigint NOT NULL,
    source_concept_id text NOT NULL,
    relation_type     text NOT NULL,
    direction         text NOT NULL DEFAULT 'directed',
    target_bundle_id  bigint,
    target_concept_id text,
    external_target   text,
    source_location   jsonb,
    confidence        double precision,
    unresolved        boolean NOT NULL DEFAULT false,
    cross_bundle      boolean NOT NULL DEFAULT false,
    provenance        jsonb,
    row_hash          text NOT NULL,
    CONSTRAINT relationship_pkey PRIMARY KEY (publication_id, ordinal),
    CONSTRAINT relationship_publication_fk
        FOREIGN KEY (publication_id)
        REFERENCES pgokf.relationship_publication (publication_id) ON DELETE CASCADE,
    CONSTRAINT relationship_direction_chk
        CHECK (direction IN ('directed', 'undirected')),
    CONSTRAINT relationship_relation_type_chk
        CHECK (length(relation_type) <= 128
               AND position(':' IN relation_type) > 1
               AND position(':' IN relation_type) < length(relation_type)),
    CONSTRAINT relationship_target_chk
        CHECK ((target_bundle_id IS NOT NULL) = (target_concept_id IS NOT NULL)
               AND NOT (target_concept_id IS NOT NULL AND external_target IS NOT NULL)
               AND (unresolved OR target_concept_id IS NOT NULL)),
    CONSTRAINT relationship_confidence_chk
        CHECK (confidence IS NULL OR (confidence >= 0 AND confidence <= 1))
);

CREATE INDEX relationship_source_type_idx
    ON pgokf.relationship (source_bundle_id, source_concept_id, relation_type);
CREATE INDEX relationship_target_type_idx
    ON pgokf.relationship (target_bundle_id, target_concept_id, relation_type)
    WHERE target_concept_id IS NOT NULL;

-- Multi-tenant isolation (see pgokf.bundles): opt-in-by-usage RLS on the
-- denormalized tenant_id. Not forced; no API role holds any grant on the raw
-- tables (readers get the pgokf.current_relationships projection only), so the
-- policies are defense in depth.
ALTER TABLE pgokf.relationship_publication ENABLE ROW LEVEL SECURITY;
CREATE POLICY relationship_publication_tenant_isolation ON pgokf.relationship_publication
    USING (((pg_catalog.current_setting('pgokf.tenant', true) IS NULL
             OR pg_catalog.current_setting('pgokf.tenant', true) = '')
            AND NOT (SELECT pgokf.tenant_required()))
        OR tenant_id = pg_catalog.current_setting('pgokf.tenant', true))
    WITH CHECK (((pg_catalog.current_setting('pgokf.tenant', true) IS NULL
                  OR pg_catalog.current_setting('pgokf.tenant', true) = '')
                 AND NOT (SELECT pgokf.tenant_required()))
        OR tenant_id = pg_catalog.current_setting('pgokf.tenant', true));

ALTER TABLE pgokf.relationship ENABLE ROW LEVEL SECURITY;
CREATE POLICY relationship_tenant_isolation ON pgokf.relationship
    USING (((pg_catalog.current_setting('pgokf.tenant', true) IS NULL
             OR pg_catalog.current_setting('pgokf.tenant', true) = '')
            AND NOT (SELECT pgokf.tenant_required()))
        OR tenant_id = pg_catalog.current_setting('pgokf.tenant', true))
    WITH CHECK (((pg_catalog.current_setting('pgokf.tenant', true) IS NULL
                  OR pg_catalog.current_setting('pgokf.tenant', true) = '')
                 AND NOT (SELECT pgokf.tenant_required()))
        OR tenant_id = pg_catalog.current_setting('pgokf.tenant', true));

REVOKE ALL ON pgokf.relationship_publication FROM PUBLIC;
REVOKE ALL ON pgokf.relationship FROM PUBLIC;

COMMENT ON TABLE pgokf.relationship_publication IS
    'Relationship publication ledger: one immutable attempt/result record per (tenant_id, producer, source_bundle_id, publication_generation), bound to a live pgokf.publication_fence slot (fencing_token) and to the catalog generation the set was computed against (expected_catalog_generation; activated_catalog_generation once active). State staged (invisible until the matching catalog generation is accepted by a refresh) / active / superseded. relationship_set_hash is the BLAKE3 digest of the canonicalized row set and doubles as the idempotency key: an identical retried replace_relationships is a no-op, a differing one under the same key is a 23505 conflict. The bundle reference detaches (ON DELETE SET NULL) so a hard deletion never erases the audit row; source_bundle_path is the durable identity snapshot. Live-key uniqueness is a partial index over attached rows only, so detached ledger rows never collide. When competing staged attempts of one producer scope expect the accepted generation, only the newest publication_generation activates. Superseded retention is bounded: an activation hard-deletes superseded publications whose updated_at is more than 30 days old - the activating bundle''s own and the detached ledger''s alike (their rows cascade) - and an unregister/purge sweeps the aged detached rows it leaves behind, so detached history never outlives the window, while the immediately previous superseded set is always retained with its rows and activation evidence. Granted to no API role.';
COMMENT ON COLUMN pgokf.relationship_publication.publication_id IS
    'Surrogate identity of the publication (GENERATED ALWAYS AS IDENTITY), the foreign-key target of pgokf.relationship.';
COMMENT ON COLUMN pgokf.relationship_publication.tenant_id IS
    'Multi-tenant owner, stamped from the source bundle at write time; part of the natural key.';
COMMENT ON COLUMN pgokf.relationship_publication.producer IS
    'Opaque caller-supplied producer label; part of the natural key. NOT authorization: replace_relationships requires session_user membership in pgokf_writer (admin inherits).';
COMMENT ON COLUMN pgokf.relationship_publication.source_bundle_id IS
    'Live reference to the bundle whose concepts are the relationship sources, or NULL after the bundle was unregistered/purged (ON DELETE SET NULL: a hard deletion never cascades away publication audit). Use source_bundle_path for the durable identity.';
COMMENT ON COLUMN pgokf.relationship_publication.source_bundle_path IS
    'Immutable snapshot of the source bundle''s canonical path (or content:<name> key) at write time; survives bundle deletion.';
COMMENT ON COLUMN pgokf.relationship_publication.publication_generation IS
    'The producer-side monotonic publication generation; equals the target_generation of the publication fence that authorized the write; part of the natural key.';
COMMENT ON COLUMN pgokf.relationship_publication.expected_catalog_generation IS
    'The pgokf.bundles.catalog_generation the row set was computed against: the current generation activates immediately; current + 1 stages for the imminent refresh; anything else is rejected (22023).';
COMMENT ON COLUMN pgokf.relationship_publication.activated_catalog_generation IS
    'The catalog generation this publication is the visible relationship set for, once active; retained as a historical record after supersession; NULL only while staged.';
COMMENT ON COLUMN pgokf.relationship_publication.fencing_token IS
    'The live fencing token of the (tenant, producer, bundle) publication fence slot at write time; a superseded or expired token is rejected, so an older producer attempt can never publish.';
COMMENT ON COLUMN pgokf.relationship_publication.relationship_set_hash IS
    'BLAKE3 hex digest of the canonicalized relationship set (rows sorted by canonical identity, per-row digests concatenated in order). Identical retried input hashes identically regardless of submission order, making replace_relationships idempotent.';
COMMENT ON COLUMN pgokf.relationship_publication.idempotency_key IS
    'The idempotency identity of the write; equal to relationship_set_hash. A retry under the same natural key with the same key is a no-op; a different key conflicts (23505).';
COMMENT ON COLUMN pgokf.relationship_publication.manifest_hash IS
    'Hash of the publication manifest the issuing fence carried (producer-supplied evidence, copied from the fence slot); NULL when the fence named none.';
COMMENT ON COLUMN pgokf.relationship_publication.state IS
    'staged (written against the next catalog generation; invisible until a refresh accepts exactly that generation), active (the current visible set for its activated generation), or superseded (replaced by a newer activation or left behind by a generation advance).';
COMMENT ON COLUMN pgokf.relationship_publication.row_count IS
    'Number of relationship rows in the set as declared at write time; 0 is a deliberate empty set (an empty replacement removes the prior set on activation). Activation-time source quarantine can remove rows, so the stored rows of an activated publication may be fewer than row_count.';
COMMENT ON COLUMN pgokf.relationship_publication.created_by IS
    'The session_user that wrote the publication, captured by column default.';
COMMENT ON COLUMN pgokf.relationship_publication.created_at IS
    'When the publication was written (transaction now()).';
COMMENT ON COLUMN pgokf.relationship_publication.activated_at IS
    'When the publication became active; retained as a historical record after supersession; NULL only while staged.';
COMMENT ON COLUMN pgokf.relationship_publication.updated_at IS
    'When this row last changed (activation or supersession); the supersession timestamp starts the 30-day window after which the next activation - or, for a detached row, the unregister/purge that detached it - prunes the superseded publication.';

COMMENT ON TABLE pgokf.relationship IS
    'The typed relationship rows of one publication (fk pgokf.relationship_publication): source concept, producer-defined namespaced relation_type (opaque text; the catalog never enumerates or interprets it), direction, the optional resolved target (target_bundle_id, target_concept_id), an optional opaque external target identifier, opaque source_location/provenance jsonb, confidence, the unresolved/cross_bundle flags, and the canonical ordinal/row hash. Rows are written once with their publication and never mutated except by activation-time target re-resolution; the one removal path is activation-time source validation, which quarantines (deletes) a staged row whose source concept does not exist in the accepted catalog generation, so a nonexistent source can never become a graph node or appear in pgokf.current_relationships. Publications are retained as audit, so rows are too, for as long as the publication itself is retained. Granted to no API role; readers use pgokf.current_relationships.';
COMMENT ON COLUMN pgokf.relationship.publication_id IS
    'The publication this row belongs to (part of the primary key).';
COMMENT ON COLUMN pgokf.relationship.ordinal IS
    'Zero-based position of the row in the canonical (sorted) order of its publication''s set, making the stored order deterministic and identical for a retried submission.';
COMMENT ON COLUMN pgokf.relationship.tenant_id IS
    'Multi-tenant owner, denormalized from the publication for a local row-level-security predicate; always equals the publication''s tenant_id.';
COMMENT ON COLUMN pgokf.relationship.source_bundle_id IS
    'Snapshot of the source bundle identity at write time (denormalized from the publication so it survives the publication''s ON DELETE SET NULL detach). Concept ids are unique only within a bundle: every graph key is (source_bundle_id, source_concept_id).';
COMMENT ON COLUMN pgokf.relationship.source_concept_id IS
    'Concept id of the relationship source within the source bundle.';
COMMENT ON COLUMN pgokf.relationship.relation_type IS
    'Producer-defined namespaced relation type (<namespace>:<name>, at most 128 characters). Opaque to the catalog: no domain relation enumeration exists and none is validated beyond the namespaced shape.';
COMMENT ON COLUMN pgokf.relationship.direction IS
    'directed (traversed source -> target; the default) or undirected (traversed both ways by concept_relationship_neighbors).';
COMMENT ON COLUMN pgokf.relationship.target_bundle_id IS
    'The resolved target''s bundle, when the endpoint validated at write (or activation) time against a bundle active and visible to the writer; NULL for external and unresolved-with-dropped-reference rows. Not a foreign key: the target identity is a snapshot and a target bundle''s later deletion must not rewrite relationship audit.';
COMMENT ON COLUMN pgokf.relationship.target_concept_id IS
    'The resolved target''s concept id within target_bundle_id, or the producer-declared target concept retained as opaque metadata on an unresolved row whose bundle IS visible but lacks the concept (when the target bundle itself was absent, inactive, or invisible to the writer, both endpoint references are dropped - the invisible and absent cases are indistinguishable); NULL for external and target-less rows.';
COMMENT ON COLUMN pgokf.relationship.external_target IS
    'Opaque producer-defined identifier of a target outside the catalog (mutually exclusive with a resolved target concept); never resolved or traversed.';
COMMENT ON COLUMN pgokf.relationship.source_location IS
    'Opaque producer-supplied jsonb locating the relationship in the producer''s own storage (never interpreted by the catalog).';
COMMENT ON COLUMN pgokf.relationship.confidence IS
    'Optional producer-supplied confidence in [0, 1]; opaque metadata.';
COMMENT ON COLUMN pgokf.relationship.unresolved IS
    'True when no live resolved target backs the row: an external target, no declared target, a target concept absent at write/activation time, or a target bundle that was absent, inactive, or invisible to the writer (the invisible and absent cases are indistinguishable - no existence leak). Unresolved rows are returned as metadata and never materialized as traversal edges.';
COMMENT ON COLUMN pgokf.relationship.cross_bundle IS
    'True when the writer declared a target in a bundle other than the source bundle (recorded from the declaration, even when the target endpoint did not resolve).';
COMMENT ON COLUMN pgokf.relationship.provenance IS
    'Opaque producer-supplied jsonb provenance (never interpreted by the catalog).';
COMMENT ON COLUMN pgokf.relationship.row_hash IS
    'BLAKE3 hex digest of the row''s canonical text; the publication''s relationship_set_hash is computed over these in ordinal order.';
",
    name = "relationship_tables",
    requires = ["catalog_tables"]
);

// The reader projection: only the active generation, with tenant and target
// visibility applied inline (the view runs with owner rights over the raw
// tables, which readers hold no grant on). A resolved row whose target bundle
// is not currently active and tenant-visible is hidden; unresolved rows stay
// as metadata. Retirement of the source bundle hides its publications without
// touching the retained audit rows.
extension_sql!(
    r"
CREATE VIEW pgokf.current_relationships AS
SELECT p.publication_id,
       p.producer,
       p.publication_generation,
       p.activated_catalog_generation AS catalog_generation,
       r.source_bundle_id,
       r.source_concept_id,
       r.relation_type,
       r.direction,
       r.target_bundle_id,
       r.target_concept_id,
       r.external_target,
       r.source_location,
       r.confidence,
       r.unresolved,
       r.cross_bundle,
       r.provenance,
       r.ordinal,
       r.row_hash,
       r.tenant_id
FROM pgokf.relationship r
JOIN pgokf.relationship_publication p ON p.publication_id = r.publication_id
JOIN pgokf.bundles sb
  ON sb.id = p.source_bundle_id AND sb.enabled AND sb.retired_at IS NULL
LEFT JOIN pgokf.bundles tb ON tb.id = r.target_bundle_id
WHERE p.state = 'active'
  AND (((pg_catalog.current_setting('pgokf.tenant', true) IS NULL
         OR pg_catalog.current_setting('pgokf.tenant', true) = '')
        AND NOT (SELECT pgokf.tenant_required()))
       OR p.tenant_id = pg_catalog.current_setting('pgokf.tenant', true))
  AND (r.unresolved
       OR r.target_bundle_id IS NULL
       OR (tb.enabled AND tb.retired_at IS NULL
           AND (((pg_catalog.current_setting('pgokf.tenant', true) IS NULL
                  OR pg_catalog.current_setting('pgokf.tenant', true) = '')
                 AND NOT (SELECT pgokf.tenant_required()))
                OR tb.tenant_id = pg_catalog.current_setting('pgokf.tenant', true))));

COMMENT ON VIEW pgokf.current_relationships IS
    'Reader projection of the current typed relationships: the rows of every ACTIVE relationship publication whose source bundle is active (enabled and not retired), tenant-scoped like the projection tables, with resolved rows hidden when their target bundle is not currently active and tenant-visible (unresolved and external rows remain as metadata and are never materialized as nonexistent references). Only the active relationship generation is exposed: staged and superseded publications stay invisible here, so no query can combine new concept content with old-generation relationships. SELECT is granted to pgokf_reader; the raw pgokf.relationship / pgokf.relationship_publication tables are granted to no API role.';
GRANT SELECT ON pgokf.current_relationships TO pgokf_reader;
",
    name = "current_relationships_view",
    requires = ["relationship_tables"]
);

// ---------------------------------------------------------------------------
// Sync-time activation (called from run_bundle_sync's tail).
// ---------------------------------------------------------------------------

/// Activate the staged publications that match a just-accepted catalog
/// generation, inside the sync transaction.
///
/// The sequence, all in one transaction under the bundle advisory lock the
/// sync caller holds:
///
/// 1. whether the bundle had relationship coverage (any active publication)
///    is recorded;
/// 2. every active publication of the bundle is superseded - it is bound to
///    the pre-refresh concepts and must never be combined with the new ones;
/// 3. staged publications expecting an older generation are superseded (their
///    refresh raced an intervening state mutation, which also advances the
///    catalog generation, so their expected generation can never be accepted);
/// 4. staged publications expecting exactly `catalog_generation` activate -
///    one per producer scope: when competing staged attempts of the same
///    (tenant, producer, bundle) scope exist, only the newest attempt (the
///    highest `publication_generation`) activates and the rest are superseded,
///    so an empty winning set really replaces the prior one;
/// 5. each activating publication's declared target concepts are re-resolved
///    against the accepted concept set (a still-absent target stays
///    `unresolved`), and every row whose source concept is absent from the
///    accepted set is quarantined (deleted) - a nonexistent source must never
///    surface as a graph node or in `current_relationships`;
/// 6. superseded publications whose supersession (`updated_at`) is more than
///    30 days old are pruned (their rows cascade) - the bundle's own and the
///    detached ledger's alike; the immediately previous superseded set is
///    always younger than the window and is retained with its rows and
///    activation evidence.
///
/// Returns whether required relationship coverage is now missing - the bundle
/// HAD coverage and none activated - so the caller marks the bundle stale with
/// the `relationship_coverage_missing` reason in the same transaction. When at
/// least one publication activates, any standing coverage-missing reason is
/// cleared (coverage was re-established).
///
/// # Errors
///
/// Returns a [`CatalogError`] on any SPI failure, aborting the surrounding
/// sync transaction so activation commits atomically with the refresh.
pub(crate) fn activate_staged(
    bundle_id: i64,
    catalog_generation: i64,
) -> Result<bool, CatalogError> {
    let had_coverage = Spi::get_one_with_args::<i64>(
        "SELECT count(*) FROM pgokf.relationship_publication
         WHERE source_bundle_id = $1 AND state = 'active'",
        &[bundle_id.into()],
    )
    .map_err(|error| spi_error("failed to count active relationship publications", &error))?
    .unwrap_or(0)
        > 0;

    Spi::run_with_args(
        "UPDATE pgokf.relationship_publication
         SET state = 'superseded', updated_at = pg_catalog.now()
         WHERE source_bundle_id = $1 AND state = 'active'",
        &[bundle_id.into()],
    )
    .map_err(|error| {
        spi_error(
            "failed to supersede active relationship publications",
            &error,
        )
    })?;

    Spi::run_with_args(
        "UPDATE pgokf.relationship_publication
         SET state = 'superseded', updated_at = pg_catalog.now()
         WHERE source_bundle_id = $1 AND state = 'staged'
           AND expected_catalog_generation < $2",
        &[bundle_id.into(), catalog_generation.into()],
    )
    .map_err(|error| spi_error("failed to supersede stale staged publications", &error))?;

    // One winner per producer scope: rank the staged attempts expecting the
    // accepted generation by publication_generation (the newest fence's
    // target); the newest activates, the rest are superseded. Both updates
    // read the same statement snapshot and touch disjoint rows.
    let activated: Vec<i64> = Spi::connect_mut(|client| {
        let table = client
            .select(
                "WITH attempts AS (
                     SELECT publication_id,
                            row_number() OVER (
                                PARTITION BY tenant_id, producer
                                ORDER BY publication_generation DESC) AS rank
                     FROM pgokf.relationship_publication
                     WHERE source_bundle_id = $1 AND state = 'staged'
                       AND expected_catalog_generation = $2),
                 superseded AS (
                     UPDATE pgokf.relationship_publication p
                     SET state = 'superseded', updated_at = pg_catalog.now()
                     FROM attempts
                     WHERE p.publication_id = attempts.publication_id
                       AND attempts.rank > 1),
                 winners AS (
                     UPDATE pgokf.relationship_publication p
                     SET state = 'active',
                         activated_catalog_generation = $2,
                         activated_at = pg_catalog.now(),
                         updated_at = pg_catalog.now()
                     FROM attempts
                     WHERE p.publication_id = attempts.publication_id
                       AND attempts.rank = 1
                     RETURNING p.publication_id)
                 SELECT publication_id FROM winners",
                None,
                &[bundle_id.into(), catalog_generation.into()],
            )
            .map_err(|error| spi_error("failed to activate staged publications", &error))?;
        let mut ids = Vec::with_capacity(table.len());
        for row in table {
            ids.push(
                RowReader::new(&row, "failed to read activated publication", "publication")
                    .required::<i64>(1, "publication_id")?,
            );
        }
        Ok::<_, CatalogError>(ids)
    })?;

    if !activated.is_empty() {
        // Activation-time target re-resolution: the staged rows were written
        // against a future concept set, so a declared target concept that the
        // accepted refresh added resolves now; a still-absent one stays
        // unresolved.
        Spi::run_with_args(
            "UPDATE pgokf.relationship r
             SET unresolved = NOT EXISTS (
                     SELECT 1 FROM pgokf.concepts c
                     WHERE c.bundle_id = r.target_bundle_id
                       AND c.id = r.target_concept_id)
             WHERE r.publication_id = ANY ($1)
               AND r.target_bundle_id IS NOT NULL
               AND r.target_concept_id IS NOT NULL",
            &[activated.clone().into()],
        )
        .map_err(|error| spi_error("failed to re-resolve relationship targets", &error))?;
        // Activation-time source validation (the immediate write path's
        // validate_concepts counterpart).
        quarantine_absent_sources(&activated)?;
        crate::catalog::freshness::clear_relationship_coverage_missing(bundle_id)?;
    }

    prune_aged_superseded(bundle_id)?;

    Ok(had_coverage && activated.is_empty())
}

/// Quarantine staged rows whose source concept the accepted concept set does
/// not contain: a nonexistent source can never become a graph node or appear
/// in `pgokf.current_relationships`.
fn quarantine_absent_sources(activated: &[i64]) -> Result<(), CatalogError> {
    Spi::run_with_args(
        "DELETE FROM pgokf.relationship r
         WHERE r.publication_id = ANY ($1)
           AND NOT EXISTS (
                 SELECT 1 FROM pgokf.concepts c
                 WHERE c.bundle_id = r.source_bundle_id
                   AND c.id = r.source_concept_id)",
        &[activated.into()],
    )
    .map_err(|error| {
        spi_error(
            "failed to quarantine absent-source relationship rows",
            &error,
        )
    })
}

// ---------------------------------------------------------------------------
// replace_relationships (writer API).
// ---------------------------------------------------------------------------

/// One fence slot row as the write path reads it.
struct FenceSlot {
    fencing_token: i64,
    state: String,
    live: bool,
    target_generation: i64,
    manifest_hash: Option<String>,
}

/// The fence slot of `(tenant, producer, bundle)`, if any.
fn read_fence_slot(
    tenant_id: &str,
    producer: &str,
    bundle_id: i64,
) -> Result<Option<FenceSlot>, CatalogError> {
    Spi::connect(|client| {
        let mut table = client
            .select(
                "SELECT fencing_token, state, expires_at > pg_catalog.now(),
                        target_generation, manifest_hash
                 FROM pgokf.publication_fence
                 WHERE tenant_id = $1 AND producer = $2 AND bundle_id = $3",
                Some(1),
                &[tenant_id.into(), producer.into(), bundle_id.into()],
            )
            .map_err(|error| spi_error("failed to read publication fence", &error))?;
        table
            .next()
            .map(|row| {
                let reader = RowReader::new(
                    &row,
                    "failed to read publication fence",
                    "publication fence",
                );
                Ok(FenceSlot {
                    fencing_token: reader.required(1, "fencing_token")?,
                    state: reader.required(2, "state")?,
                    live: reader.required(3, "live")?,
                    target_generation: reader.required(4, "target_generation")?,
                    manifest_hash: reader.optional(5)?,
                })
            })
            .transpose()
    })
}

/// One existing publication's idempotency identity.
struct ExistingPublication {
    publication_id: i64,
    relationship_set_hash: String,
}

/// Look up the publication under one natural key, if it exists.
fn read_existing_publication(
    tenant_id: &str,
    producer: &str,
    source_bundle_id: i64,
    publication_generation: i64,
) -> Result<Option<ExistingPublication>, CatalogError> {
    Spi::connect(|client| {
        let mut table = client
            .select(
                "SELECT publication_id, relationship_set_hash
                 FROM pgokf.relationship_publication
                 WHERE tenant_id = $1 AND producer = $2
                   AND source_bundle_id = $3 AND publication_generation = $4",
                Some(1),
                &[
                    tenant_id.into(),
                    producer.into(),
                    source_bundle_id.into(),
                    publication_generation.into(),
                ],
            )
            .map_err(|error| spi_error("failed to read relationship publication", &error))?;
        table
            .next()
            .map(|row| {
                let reader = RowReader::new(
                    &row,
                    "failed to read relationship publication",
                    "relationship publication",
                );
                Ok(ExistingPublication {
                    publication_id: reader.required(1, "publication_id")?,
                    relationship_set_hash: reader.required(2, "relationship_set_hash")?,
                })
            })
            .transpose()
    })
}

/// Supersede the producer's active publications for the bundle (the write
/// path's immediate activation replaces them in the same transaction).
fn supersede_active(
    tenant_id: &str,
    producer: &str,
    source_bundle_id: i64,
) -> Result<(), CatalogError> {
    Spi::run_with_args(
        "UPDATE pgokf.relationship_publication
         SET state = 'superseded', updated_at = pg_catalog.now()
         WHERE tenant_id = $1 AND producer = $2 AND source_bundle_id = $3
           AND state = 'active'",
        &[tenant_id.into(), producer.into(), source_bundle_id.into()],
    )
    .map_err(|error| spi_error("failed to supersede prior publications", &error))
}

/// Bound superseded retention: hard-delete superseded publications whose
/// supersession (`updated_at`) is more than 30 days old - the acknowledged
/// change-event outbox precedent - cascading their rows. Runs at every
/// activation (immediate or refresh-time) and sweeps both the activating
/// bundle's aged superseded rows and the detached ledger (`source_bundle_id
/// IS NULL` after an unregister/purge), so aged history stays reachable by
/// retention even after its bundle leaves the catalog. The sweep is global
/// retention, not visibility: it is deliberately not tenant-predicated (the
/// change-event retention prune precedent). The immediately previous
/// superseded set is by construction younger than the window, so its rows
/// and activation evidence always survive.
fn prune_aged_superseded(source_bundle_id: i64) -> Result<(), CatalogError> {
    Spi::run_with_args(
        "DELETE FROM pgokf.relationship_publication
         WHERE (source_bundle_id = $1 OR source_bundle_id IS NULL)
           AND state = 'superseded'
           AND updated_at < pg_catalog.now() - pg_catalog.make_interval(days => 30)",
        &[source_bundle_id.into()],
    )
    .map_err(|error| spi_error("failed to prune aged superseded publications", &error))
}

/// The detach-time half of superseded retention: hard-delete aged superseded
/// publications whose bundle reference was detached (`source_bundle_id IS
/// NULL` by an unregister/purge in this transaction), cascading their rows.
/// Called by `unregister_bundle`/`purge_retired` after the delete detaches
/// the ledger, so aged superseded history is pruned even if no activation
/// ever runs again. Only aged `superseded` rows are touched: detached
/// `active` rows are the retained audit ledger, and recently superseded
/// rows stay inside the 30-day evidence window. Global retention, not
/// visibility: deliberately not tenant-predicated.
///
/// # Errors
///
/// Returns a [`CatalogError`] on any SPI failure.
pub(crate) fn prune_aged_detached_superseded() -> Result<(), CatalogError> {
    Spi::run_with_args(
        "DELETE FROM pgokf.relationship_publication
         WHERE source_bundle_id IS NULL AND state = 'superseded'
           AND updated_at < pg_catalog.now() - pg_catalog.make_interval(days => 30)",
        &[],
    )
    .map_err(|error| spi_error("failed to prune aged detached publications", &error))
}

/// Insert the publication row, returning its surrogate identity.
#[allow(clippy::too_many_arguments)]
fn insert_publication(
    tenant_id: &str,
    producer: &str,
    source_bundle_id: i64,
    source_bundle_path: &str,
    publication_generation: i64,
    expected_catalog_generation: i64,
    fencing_token: i64,
    set_hash: &str,
    manifest_hash: Option<&str>,
    activate: bool,
    row_count: i32,
) -> Result<i64, CatalogError> {
    Spi::get_one_with_args::<i64>(
        "INSERT INTO pgokf.relationship_publication
             (tenant_id, producer, source_bundle_id, source_bundle_path,
              publication_generation, expected_catalog_generation,
              activated_catalog_generation, fencing_token,
              relationship_set_hash, idempotency_key, manifest_hash,
              state, row_count, activated_at)
         VALUES ($1, $2, $3, $4, $5, $6,
                 CASE WHEN $11 THEN $6 END,
                 $7, $8, $8, $9,
                 CASE WHEN $11 THEN 'active' ELSE 'staged' END,
                 $10,
                 CASE WHEN $11 THEN pg_catalog.now() END)
         RETURNING publication_id",
        &[
            tenant_id.into(),
            producer.into(),
            source_bundle_id.into(),
            source_bundle_path.into(),
            publication_generation.into(),
            expected_catalog_generation.into(),
            fencing_token.into(),
            set_hash.into(),
            manifest_hash.into(),
            row_count.into(),
            activate.into(),
        ],
    )
    .map_err(|error| spi_error("failed to insert relationship publication", &error))?
    .ok_or_else(|| {
        CatalogError::internal(
            "relationship publication insert returned no id",
            Path::new(""),
        )
    })
}

/// Bulk-insert one publication's rows with one array-unnest `INSERT` per
/// [`BATCH_SIZE`] chunk (the bounded projection-write discipline of
/// [`crate::catalog::links`]). The opaque jsonb fields travel as text and are
/// cast per row.
fn insert_rows(
    publication_id: i64,
    tenant_id: &str,
    source_bundle_id: i64,
    hashed_rows: &[(RelationshipRow, String)],
) -> Result<(), CatalogError> {
    const INSERT: &str = "
        INSERT INTO pgokf.relationship
            (publication_id, ordinal, tenant_id, source_bundle_id, source_concept_id,
             relation_type, direction, target_bundle_id, target_concept_id,
             external_target, source_location, confidence, unresolved, cross_bundle,
             provenance, row_hash)
        SELECT $1, d.ordinal, $2, $3, d.source_concept_id, d.relation_type,
               d.direction, d.target_bundle_id, d.target_concept_id, d.external_target,
               d.source_location::pg_catalog.jsonb, d.confidence, d.unresolved,
               d.cross_bundle, d.provenance::pg_catalog.jsonb, d.row_hash
        FROM unnest($4::integer[], $5::text[], $6::text[], $7::text[], $8::bigint[],
                    $9::text[], $10::text[], $11::text[], $12::double precision[],
                    $13::boolean[], $14::boolean[], $15::text[], $16::text[])
             AS d(ordinal, source_concept_id, relation_type, direction, target_bundle_id,
                  target_concept_id, external_target, source_location, confidence,
                  unresolved, cross_bundle, provenance, row_hash)";

    let total = hashed_rows.len();
    for start in (0..total).step_by(BATCH_SIZE) {
        let end = usize::min(start + BATCH_SIZE, total);
        let chunk = &hashed_rows[start..end];
        let ordinals: Vec<i32> = (0..chunk.len())
            .map(|offset| i32::try_from(start + offset).unwrap_or(i32::MAX))
            .collect();
        let source_ids: Vec<&str> = chunk
            .iter()
            .map(|(row, _)| row.source_concept_id.as_str())
            .collect();
        let relation_types: Vec<&str> = chunk
            .iter()
            .map(|(row, _)| row.relation_type.as_str())
            .collect();
        let directions: Vec<&str> = chunk
            .iter()
            .map(|(row, _)| row.direction.as_str())
            .collect();
        let target_bundles: Vec<Option<i64>> =
            chunk.iter().map(|(row, _)| row.target_bundle_id).collect();
        let target_concepts: Vec<Option<&str>> = chunk
            .iter()
            .map(|(row, _)| row.target_concept_id.as_deref())
            .collect();
        let external_targets: Vec<Option<&str>> = chunk
            .iter()
            .map(|(row, _)| row.external_target.as_deref())
            .collect();
        let locations: Vec<Option<&str>> = chunk
            .iter()
            .map(|(row, _)| row.source_location.as_deref())
            .collect();
        let confidences: Vec<Option<f64>> = chunk.iter().map(|(row, _)| row.confidence).collect();
        let unresolveds: Vec<bool> = chunk.iter().map(|(row, _)| row.unresolved).collect();
        let cross_bundles: Vec<bool> = chunk.iter().map(|(row, _)| row.cross_bundle).collect();
        let provenances: Vec<Option<&str>> = chunk
            .iter()
            .map(|(row, _)| row.provenance.as_deref())
            .collect();
        let row_hashes: Vec<&str> = chunk.iter().map(|(_, hash)| hash.as_str()).collect();
        Spi::run_with_args(
            INSERT,
            &[
                publication_id.into(),
                tenant_id.into(),
                source_bundle_id.into(),
                ordinals.into(),
                source_ids.into(),
                relation_types.into(),
                directions.into(),
                target_bundles.into(),
                target_concepts.into(),
                external_targets.into(),
                locations.into(),
                confidences.into(),
                unresolveds.into(),
                cross_bundles.into(),
                provenances.into(),
                row_hashes.into(),
            ],
        )
        .map_err(|error| spi_error("failed to insert relationship rows", &error))?;
    }
    Ok(())
}

/// One publication projected onto the `pgokf.relationship_publication_info`
/// shape.
struct PublicationInfo {
    publication_id: i64,
    tenant_id: String,
    producer: String,
    source_bundle_id: Option<i64>,
    source_bundle_path: String,
    publication_generation: i64,
    expected_catalog_generation: i64,
    activated_catalog_generation: Option<i64>,
    fencing_token: i64,
    relationship_set_hash: String,
    manifest_hash: Option<String>,
    state: String,
    row_count: i32,
    idempotency_key: String,
    created_at: TimestampWithTimeZone,
    activated_at: Option<TimestampWithTimeZone>,
}

/// Load a publication by surrogate identity, projected for the return type.
fn load_publication_info(publication_id: i64) -> Result<PublicationInfo, CatalogError> {
    Spi::connect(|client| {
        let mut table = client
            .select(
                "SELECT publication_id, tenant_id, producer, source_bundle_id,
                        source_bundle_path, publication_generation,
                        expected_catalog_generation, activated_catalog_generation,
                        fencing_token, relationship_set_hash, manifest_hash, state,
                        row_count, idempotency_key, created_at, activated_at
                 FROM pgokf.relationship_publication
                 WHERE publication_id = $1",
                Some(1),
                &[publication_id.into()],
            )
            .map_err(|error| spi_error("failed to load relationship publication", &error))?;
        let Some(row) = table.next() else {
            return Err(CatalogError::internal(
                "relationship publication vanished after write",
                Path::new(""),
            ));
        };
        let reader = RowReader::new(
            &row,
            "failed to read relationship publication",
            "relationship publication",
        );
        Ok(PublicationInfo {
            publication_id: reader.required(1, "publication_id")?,
            tenant_id: reader.required(2, "tenant_id")?,
            producer: reader.required(3, "producer")?,
            source_bundle_id: reader.optional(4)?,
            source_bundle_path: reader.required(5, "source_bundle_path")?,
            publication_generation: reader.required(6, "publication_generation")?,
            expected_catalog_generation: reader.required(7, "expected_catalog_generation")?,
            activated_catalog_generation: reader.optional(8)?,
            fencing_token: reader.required(9, "fencing_token")?,
            relationship_set_hash: reader.required(10, "relationship_set_hash")?,
            manifest_hash: reader.optional(11)?,
            state: reader.required(12, "state")?,
            row_count: reader.required(13, "row_count")?,
            idempotency_key: reader.required(14, "idempotency_key")?,
            created_at: reader.required(15, "created_at")?,
            activated_at: reader.optional(16)?,
        })
    })
}

fn composite_error(error: impl std::fmt::Display) -> CatalogError {
    CatalogError::internal(
        format!("failed to build relationship composite: {error}"),
        Path::new(""),
    )
}

fn publication_info_tuple(
    info: &PublicationInfo,
) -> Result<PgHeapTuple<'static, AllocatedByRust>, CatalogError> {
    let mut tuple = PgHeapTuple::new_composite_type("pgokf.relationship_publication_info")
        .map_err(composite_error)?;
    tuple
        .set_by_name("publication_id", info.publication_id)
        .map_err(composite_error)?;
    tuple
        .set_by_name("tenant_id", info.tenant_id.clone())
        .map_err(composite_error)?;
    tuple
        .set_by_name("producer", info.producer.clone())
        .map_err(composite_error)?;
    tuple
        .set_by_name("source_bundle_id", info.source_bundle_id)
        .map_err(composite_error)?;
    tuple
        .set_by_name("source_bundle_path", info.source_bundle_path.clone())
        .map_err(composite_error)?;
    tuple
        .set_by_name("publication_generation", info.publication_generation)
        .map_err(composite_error)?;
    tuple
        .set_by_name(
            "expected_catalog_generation",
            info.expected_catalog_generation,
        )
        .map_err(composite_error)?;
    tuple
        .set_by_name(
            "activated_catalog_generation",
            info.activated_catalog_generation,
        )
        .map_err(composite_error)?;
    tuple
        .set_by_name("fencing_token", info.fencing_token)
        .map_err(composite_error)?;
    tuple
        .set_by_name("relationship_set_hash", info.relationship_set_hash.clone())
        .map_err(composite_error)?;
    tuple
        .set_by_name("manifest_hash", info.manifest_hash.clone())
        .map_err(composite_error)?;
    tuple
        .set_by_name("state", info.state.clone())
        .map_err(composite_error)?;
    tuple
        .set_by_name("row_count", info.row_count)
        .map_err(composite_error)?;
    tuple
        .set_by_name("idempotency_key", info.idempotency_key.clone())
        .map_err(composite_error)?;
    tuple
        .set_by_name("created_at", info.created_at)
        .map_err(composite_error)?;
    tuple
        .set_by_name("activated_at", info.activated_at)
        .map_err(composite_error)?;
    Ok(tuple)
}

/// The result of the fence compare-and-set: the live slot, whose
/// `manifest_hash` evidence the publication inherits.
///
/// The token must be the slot's live, unexpired one and the publication
/// generation must equal the fence's target, so a superseded or expired
/// producer attempt can never publish (SQLSTATE `22023` otherwise).
fn checked_fence(
    tenant_id: &str,
    producer: &str,
    source_bundle_id: i64,
    publication_generation: i64,
    fencing_token: i64,
) -> Result<FenceSlot, CatalogError> {
    let fence = read_fence_slot(tenant_id, producer, source_bundle_id)?.ok_or_else(|| {
        CatalogError::invalid_parameter(
            format!(
                "no publication fence for producer {producer} on bundle {source_bundle_id}; \
                 issue one with pgokf.issue_publication_fence first"
            ),
            Path::new(""),
        )
    })?;
    if fence.state != "issued" || !fence.live || fence.fencing_token != fencing_token {
        return Err(CatalogError::invalid_parameter(
            format!(
                "no live publication fence for producer {producer} on bundle {source_bundle_id} \
                 with fencing_token {fencing_token} (the token is superseded, expired, or never \
                 issued)"
            ),
            Path::new(""),
        ));
    }
    if fence.target_generation != publication_generation {
        return Err(CatalogError::invalid_parameter(
            format!(
                "publication_generation {publication_generation} does not match the live fence's \
                 target generation {}; re-issue the fence for this publication",
                fence.target_generation
            ),
            Path::new(""),
        ));
    }
    Ok(fence)
}

/// Whether the write activates immediately (`expected` = the current
/// generation) or stages for the imminent refresh (`expected` = the next).
/// Anything else is rejected: a write computed against a stale catalog
/// generation never becomes visible.
fn activation_mode(expected: i64, current: i64) -> Result<bool, CatalogError> {
    if expected == current {
        Ok(true)
    } else if expected == current + 1 {
        Ok(false)
    } else {
        Err(CatalogError::invalid_parameter(
            format!(
                "expected_catalog_generation {expected} does not match the bundle's current \
                 catalog generation {current} (activate) or its next generation {} (stage for \
                 the pending refresh); re-read the catalog and retry",
                current + 1
            ),
            Path::new(""),
        ))
    }
}

/// The full `replace_relationships` write path; see the module docs for the
/// exact generation/fence semantics.
// needless_pass_by_value: the pg_extern ABI hands `rows` over by value, and
// parse_rows only borrows it.
#[allow(clippy::needless_pass_by_value)]
fn replace_relationships_impl(
    producer: &str,
    source_bundle_id: i64,
    publication_generation: i64,
    expected_catalog_generation: i64,
    fencing_token: i64,
    rows: JsonB,
) -> Result<PublicationInfo, CatalogError> {
    security::authorize_current_user(security::Operation::Ingest, Path::new(""))?;
    if producer.trim().is_empty() {
        return Err(CatalogError::invalid_parameter(
            "producer must not be empty",
            Path::new(""),
        ));
    }
    if publication_generation <= 0 {
        return Err(CatalogError::invalid_parameter(
            format!("publication_generation must be greater than 0, got {publication_generation}"),
            Path::new(""),
        ));
    }
    if expected_catalog_generation < 0 {
        return Err(CatalogError::invalid_parameter(
            format!(
                "expected_catalog_generation must be at least 0, got {expected_catalog_generation}"
            ),
            Path::new(""),
        ));
    }
    security::enforce_bundle_tenant(source_bundle_id)?;

    // Parse, validate, deduplicate, and canonicalize BEFORE touching the
    // catalog, so a malformed submission fails without taking the lock.
    let mut parsed = parse_rows(&rows)?;
    reject_duplicates(&parsed)?;

    let stored_path = crate::catalog::freshness::bundle_path(source_bundle_id)?;
    let key = advisory_lock_key(&stored_path);
    Spi::run_with_args("SELECT pg_catalog.pg_advisory_xact_lock($1)", &[key.into()])
        .map_err(|error| spi_error("failed to acquire bundle advisory lock", &error))?;

    let (tenant_id, catalog_generation) = Spi::connect(|client| {
        let table = client
            .select(
                "SELECT tenant_id, catalog_generation FROM pgokf.bundles WHERE id = $1",
                Some(1),
                &[source_bundle_id.into()],
            )
            .map_err(|error| spi_error("failed to read bundle generation", &error))?;
        let Some(row) = table.into_iter().next() else {
            return Err(unknown_bundle_error(source_bundle_id));
        };
        let reader = RowReader::new(&row, "failed to read bundle row", "bundle");
        Ok((
            reader.required::<String>(1, "tenant_id")?,
            reader.required::<i64>(2, "catalog_generation")?,
        ))
    })?;

    // Fence check: the compare-and-set against the live slot.
    let fence = checked_fence(
        &tenant_id,
        producer,
        source_bundle_id,
        publication_generation,
        fencing_token,
    )?;

    // Generation rule: the current generation activates immediately, the next
    // one stages for the imminent refresh, anything else is rejected.
    let activate = activation_mode(expected_catalog_generation, catalog_generation)?;

    resolve_endpoints(source_bundle_id, &mut parsed, activate)?;
    let (hashed_rows, set_hash) = canonicalize(parsed);

    // Idempotency: the same natural key with the same canonical set is a
    // no-op returning the existing publication; the same key with a different
    // set is a conflict.
    if let Some(existing) = read_existing_publication(
        &tenant_id,
        producer,
        source_bundle_id,
        publication_generation,
    )? {
        if existing.relationship_set_hash == set_hash {
            return load_publication_info(existing.publication_id);
        }
        return Err(CatalogError::duplicate_path(
            format!(
                "publication ({producer}, bundle {source_bundle_id}, generation \
                 {publication_generation}) already exists with a different relationship set; \
                 choose a new publication_generation (re-issue the fence)"
            ),
            Path::new(""),
        ));
    }

    if activate {
        supersede_active(&tenant_id, producer, source_bundle_id)?;
    }
    let row_count = i32::try_from(hashed_rows.len()).unwrap_or(i32::MAX);
    let publication_id = insert_publication(
        &tenant_id,
        producer,
        source_bundle_id,
        &stored_path,
        publication_generation,
        expected_catalog_generation,
        fencing_token,
        &set_hash,
        fence.manifest_hash.as_deref(),
        activate,
        row_count,
    )?;
    insert_rows(publication_id, &tenant_id, source_bundle_id, &hashed_rows)?;
    if activate {
        // Immediate activation re-establishes coverage; a standing
        // coverage-missing reason from a prior refresh clears.
        crate::catalog::freshness::clear_relationship_coverage_missing(source_bundle_id)?;
        prune_aged_superseded(source_bundle_id)?;
    }
    load_publication_info(publication_id)
}

// ---------------------------------------------------------------------------
// concept_relationship_neighbors (typed traversal, reader API).
// ---------------------------------------------------------------------------

/// One visited node's shortest-path record while the traversal runs.
struct TypedVisit {
    hops: i32,
    path_bundle_ids: Vec<i64>,
    path_concept_ids: Vec<String>,
    /// The relation type of the edge that first (minimum-hop) reached the node.
    relation_type: String,
}

/// One edge of the typed graph: `(from) -> (to)` with its relation type.
struct TypedEdge {
    from: (i64, String),
    to: (i64, String),
    relation_type: String,
}

/// The visited set as the BFS yields it: each discovered node with its
/// shortest-path record.
type TypedVisits = Vec<((i64, String), TypedVisit)>;

/// Cycle-safe, set-based level BFS over typed edges keyed on
/// `(bundle_id, concept_id)` - the [`crate::catalog::neighbors`] algorithm with
/// composite node keys. Records the first (minimum-hop) visit of each node and
/// never re-expands a visited node, so total work is `O(V + E)` and cycles
/// terminate. Discovery order follows the edges' sorted order, so results are
/// deterministic; discovery stops once `max_results` nodes are recorded.
fn typed_breadth_first<F>(
    seed: (i64, String),
    max_hops: i32,
    max_results: usize,
    mut edges_from: F,
) -> Result<TypedVisits, CatalogError>
where
    F: FnMut(&[(i64, String)]) -> Result<Vec<TypedEdge>, CatalogError>,
{
    let mut visited: HashMap<(i64, String), TypedVisit> = HashMap::new();
    visited.insert(
        seed.clone(),
        TypedVisit {
            hops: 0,
            path_bundle_ids: vec![seed.0],
            path_concept_ids: vec![seed.1.clone()],
            relation_type: String::new(),
        },
    );
    let mut discovered: Vec<(i64, String)> = Vec::new();
    let mut frontier: Vec<(i64, String)> = vec![seed];
    let mut hop: i32 = 1;

    while hop <= max_hops && !frontier.is_empty() && discovered.len() < max_results {
        let edges = edges_from(&frontier)?;
        let mut next: Vec<(i64, String)> = Vec::new();
        for edge in edges {
            if discovered.len() >= max_results {
                break;
            }
            if visited.contains_key(&edge.to) {
                continue;
            }
            // `from` is always already visited (it is a frontier member), so
            // its shortest path is available to extend by one edge.
            let from_record = visited
                .get(&edge.from)
                .expect("an edge's source is always a visited frontier member");
            let mut path_bundle_ids = from_record.path_bundle_ids.clone();
            let mut path_concept_ids = from_record.path_concept_ids.clone();
            path_bundle_ids.push(edge.to.0);
            path_concept_ids.push(edge.to.1.clone());
            visited.insert(
                edge.to.clone(),
                TypedVisit {
                    hops: hop,
                    path_bundle_ids,
                    path_concept_ids,
                    relation_type: edge.relation_type,
                },
            );
            discovered.push(edge.to.clone());
            next.push(edge.to);
        }
        frontier = next;
        hop += 1;
    }

    Ok(discovered
        .into_iter()
        .map(|node| {
            let record = visited
                .remove(&node)
                .expect("a discovered neighbor is always recorded in visited");
            (node, record)
        })
        .collect())
}

/// The out-edge expansion of one frontier level: forward edges from frontier
/// nodes, plus undirected edges whose target is a frontier node, followed in
/// reverse (an undirected edge is traversable both ways, outbound included -
/// the mirror of the inbound side's undirected handling). Sorted for
/// deterministic discovery order.
const FORWARD_EDGE_QUERY: &str = "
    SELECT r.source_bundle_id, r.source_concept_id, r.target_bundle_id,
           r.target_concept_id, r.relation_type
    FROM pgokf.current_relationships r
    JOIN (SELECT * FROM unnest($1::bigint[], $2::text[])) AS f(bundle_id, concept_id)
      ON r.source_bundle_id = f.bundle_id AND r.source_concept_id = f.concept_id
    WHERE NOT r.unresolved
      AND ($3::text[] IS NULL OR r.relation_type = ANY ($3))
    UNION ALL
    SELECT r.target_bundle_id, r.target_concept_id, r.source_bundle_id,
           r.source_concept_id, r.relation_type
    FROM pgokf.current_relationships r
    JOIN (SELECT * FROM unnest($1::bigint[], $2::text[])) AS f(bundle_id, concept_id)
      ON r.target_bundle_id = f.bundle_id AND r.target_concept_id = f.concept_id
    WHERE NOT r.unresolved AND r.direction = 'undirected'
      AND ($3::text[] IS NULL OR r.relation_type = ANY ($3))
    ORDER BY 1, 2, 3, 4, 5";

/// The in-edge expansion of one frontier level: resolved edges whose target is
/// a frontier node (neighbor = the edge's source), plus undirected edges
/// leaving a frontier node (an undirected edge is traversable both ways).
/// Sorted; the `backward` flag distinguishes the two shapes for the caller.
const BACKWARD_EDGE_QUERY: &str = "
    SELECT r.target_bundle_id, r.target_concept_id, r.source_bundle_id,
           r.source_concept_id, r.relation_type
    FROM pgokf.current_relationships r
    JOIN (SELECT * FROM unnest($1::bigint[], $2::text[])) AS f(bundle_id, concept_id)
      ON r.target_bundle_id = f.bundle_id AND r.target_concept_id = f.concept_id
    WHERE NOT r.unresolved
      AND ($3::text[] IS NULL OR r.relation_type = ANY ($3))
    UNION ALL
    SELECT r.source_bundle_id, r.source_concept_id, r.target_bundle_id,
           r.target_concept_id, r.relation_type
    FROM pgokf.current_relationships r
    JOIN (SELECT * FROM unnest($1::bigint[], $2::text[])) AS f(bundle_id, concept_id)
      ON r.source_bundle_id = f.bundle_id AND r.source_concept_id = f.concept_id
    WHERE NOT r.unresolved AND r.direction = 'undirected'
      AND ($3::text[] IS NULL OR r.relation_type = ANY ($3))
    ORDER BY 1, 2, 3, 4, 5";

/// Read one edge-query row into a [`TypedEdge`].
fn read_edge(row: &SpiHeapTupleData<'_>) -> Result<TypedEdge, CatalogError> {
    let reader = RowReader::new(row, "failed to read relationship edge", "relationship");
    Ok(TypedEdge {
        from: (
            reader.required(1, "from_bundle_id")?,
            reader.required(2, "from_concept_id")?,
        ),
        to: (
            reader.required(3, "to_bundle_id")?,
            reader.required(4, "to_concept_id")?,
        ),
        relation_type: reader.required(5, "relation_type")?,
    })
}

/// Fetch one hop level's edges for a whole frontier in the requested direction
/// mode, via the set-based queries. Returned edges are sorted.
fn expand_typed_frontier(
    frontier: &[(i64, String)],
    direction: &str,
    relation_types: Option<&[String]>,
) -> Result<Vec<TypedEdge>, CatalogError> {
    let bundle_ids: Vec<i64> = frontier.iter().map(|(bundle, _)| *bundle).collect();
    let concept_ids: Vec<&str> = frontier.iter().map(|(_, id)| id.as_str()).collect();
    let types: Option<Vec<String>> = relation_types.map(<[String]>::to_vec);
    Spi::connect(|client| {
        let mut edges = Vec::new();
        if matches!(direction, "outbound" | "both") {
            let table = client
                .select(
                    FORWARD_EDGE_QUERY,
                    None,
                    &[
                        bundle_ids.clone().into(),
                        concept_ids.clone().into(),
                        types.clone().into(),
                    ],
                )
                .map_err(|error| spi_error("relationship traversal edge query failed", &error))?;
            for row in table {
                edges.push(read_edge(&row)?);
            }
        }
        if matches!(direction, "inbound" | "both") {
            let table = client
                .select(
                    BACKWARD_EDGE_QUERY,
                    None,
                    &[bundle_ids.into(), concept_ids.into(), types.into()],
                )
                .map_err(|error| spi_error("relationship traversal edge query failed", &error))?;
            for row in table {
                edges.push(read_edge(&row)?);
            }
        }
        Ok(edges)
    })
}

/// One discovered neighbor with its annotations, as `pgokf.relationship_neighbor`.
struct TypedNeighborHit {
    start_bundle_id: i64,
    start_concept_id: String,
    bundle_id: i64,
    concept_id: String,
    hops: i32,
    path_bundle_ids: Vec<i64>,
    path_concept_ids: Vec<String>,
    relation_type: String,
    title: Option<String>,
    freshness_state: String,
    freshness_reasons: Vec<String>,
    freshness_scope: String,
    stale_since: Option<TimestampWithTimeZone>,
    observed_revision: Option<String>,
    indexed_revision: Option<String>,
    published_revision: Option<String>,
    catalog_generation: i64,
    last_reconciled_at: Option<TimestampWithTimeZone>,
    embedding_state: String,
    embedding_model: Option<String>,
    embedding_dim: Option<i32>,
    embedding_input_hash: Option<String>,
    embedded_at: Option<TimestampWithTimeZone>,
}

/// The neighbor annotation statement: the discovered nodes joined to their
/// concepts (active bundles only; a vanished concept drops the node, the
/// inner-join parity of [`crate::catalog::neighbors`]) and annotated with the
/// effective freshness (concept > path > bundle precedence) in the inner
/// projection, then with the embedding provenance in the outer one, which
/// re-joins `pgokf.concepts` as `c` for the contract predicate - the same
/// shape `pgokf.concept_search_fresh` produces. `contract_match` is
/// [`crate::catalog::embedding::contract_match_sql`] with the policy bound as
/// `$3` (model), `$4` (dimension), and `$5` (contract).
fn neighbor_annotation_statement(contract_match: &str) -> String {
    format!(
        "
    SELECT ranked.bundle_id,
           ranked.concept_id,
           ranked.title,
           ranked.freshness_state,
           ranked.freshness_reasons,
           ranked.freshness_scope,
           ranked.stale_since,
           ranked.observed_revision,
           ranked.indexed_revision,
           ranked.published_revision,
           ranked.catalog_generation,
           ranked.last_reconciled_at,
           CASE WHEN e.concept_id IS NULL THEN 'missing'
                WHEN ranked.freshness_state = 'fresh' AND {contract_match} THEN 'current'
                ELSE 'stale' END AS embedding_state,
           e.model AS embedding_model,
           e.dim AS embedding_dim,
           e.input_hash AS embedding_input_hash,
           e.updated_at AS embedded_at
    FROM (
    SELECT c.bundle_id,
           c.id AS concept_id,
           c.title,
           COALESCE(fc.state, fp.state, fb.state, 'fresh') AS freshness_state,
           COALESCE(fc.reasons, fp.reasons, fb.reasons, '{{}}'::text[]) AS freshness_reasons,
           COALESCE(fc.scope_kind || ':' || fc.scope_key,
                    fp.scope_kind || ':' || fp.scope_key,
                    'bundle') AS freshness_scope,
           COALESCE(fc.stale_since, fp.stale_since, fb.stale_since) AS stale_since,
           COALESCE(fc.observed_revision, fp.observed_revision, fb.observed_revision)
               AS observed_revision,
           COALESCE(fc.indexed_revision, fp.indexed_revision, fb.indexed_revision)
               AS indexed_revision,
           COALESCE(fc.published_revision, fp.published_revision, fb.published_revision)
               AS published_revision,
           b.catalog_generation,
           COALESCE(fc.last_reconciled_at, fp.last_reconciled_at, fb.last_reconciled_at)
               AS last_reconciled_at
    FROM (SELECT * FROM unnest($1::bigint[], $2::text[])) AS d(bundle_id, concept_id)
    JOIN pgokf.concepts c ON c.bundle_id = d.bundle_id AND c.id = d.concept_id
    JOIN pgokf.bundles b ON b.id = c.bundle_id AND b.enabled AND b.retired_at IS NULL
    LEFT JOIN pgokf.effective_freshness fc
           ON fc.bundle_id = c.bundle_id
          AND fc.scope_kind = 'concept' AND fc.scope_key = c.id
    LEFT JOIN pgokf.effective_freshness fp
           ON fp.bundle_id = c.bundle_id
          AND fp.scope_kind = 'path' AND fp.scope_key = c.path
    LEFT JOIN pgokf.effective_freshness fb
           ON fb.bundle_id = c.bundle_id AND fb.scope_kind = 'bundle'
    ) AS ranked
    JOIN pgokf.concepts c ON c.bundle_id = ranked.bundle_id AND c.id = ranked.concept_id
    LEFT JOIN pgokf.concept_embedding e
           ON e.bundle_id = ranked.bundle_id AND e.concept_id = ranked.concept_id"
    )
}

/// The freshness/embedding annotation of one discovered node.
struct NodeAnnotation {
    title: Option<String>,
    freshness_state: String,
    freshness_reasons: Vec<String>,
    freshness_scope: String,
    stale_since: Option<TimestampWithTimeZone>,
    observed_revision: Option<String>,
    indexed_revision: Option<String>,
    published_revision: Option<String>,
    catalog_generation: i64,
    last_reconciled_at: Option<TimestampWithTimeZone>,
    embedding_state: String,
    embedding_model: Option<String>,
    embedding_dim: Option<i32>,
    embedding_input_hash: Option<String>,
    embedded_at: Option<TimestampWithTimeZone>,
}

/// Annotate the discovered nodes in one set-based query. A node absent from
/// the result has a vanished concept (or inactive bundle) and is dropped.
fn annotate_nodes(
    nodes: &[(i64, String)],
    policy: &crate::catalog::embedding::EmbeddingPolicy,
) -> Result<HashMap<(i64, String), NodeAnnotation>, CatalogError> {
    let bundle_ids: Vec<i64> = nodes.iter().map(|(bundle, _)| *bundle).collect();
    let concept_ids: Vec<&str> = nodes.iter().map(|(_, id)| id.as_str()).collect();
    let statement =
        neighbor_annotation_statement(&crate::catalog::embedding::contract_match_sql(3, 4, 5));
    Spi::connect(|client| {
        let table = client
            .select(
                statement.as_str(),
                None,
                &[
                    bundle_ids.into(),
                    concept_ids.into(),
                    policy.model.clone().into(),
                    policy.dim.into(),
                    policy.contract.clone().into(),
                ],
            )
            .map_err(|error| spi_error("relationship neighbor annotation query failed", &error))?;
        let mut annotations = HashMap::with_capacity(table.len());
        for row in table {
            let reader = RowReader::new(
                &row,
                "failed to read relationship neighbor annotation",
                "relationship_neighbor",
            );
            let node = (
                reader.required::<i64>(1, "bundle_id")?,
                reader.required::<String>(2, "concept_id")?,
            );
            annotations.insert(
                node,
                NodeAnnotation {
                    title: reader.optional(3)?,
                    freshness_state: reader.required(4, "freshness_state")?,
                    freshness_reasons: reader.required(5, "freshness_reasons")?,
                    freshness_scope: reader.required(6, "freshness_scope")?,
                    stale_since: reader.optional(7)?,
                    observed_revision: reader.optional(8)?,
                    indexed_revision: reader.optional(9)?,
                    published_revision: reader.optional(10)?,
                    catalog_generation: reader.required(11, "catalog_generation")?,
                    last_reconciled_at: reader.optional(12)?,
                    embedding_state: reader.required(13, "embedding_state")?,
                    embedding_model: reader.optional(14)?,
                    embedding_dim: reader.optional(15)?,
                    embedding_input_hash: reader.optional(16)?,
                    embedded_at: reader.optional(17)?,
                },
            );
        }
        Ok(annotations)
    })
}

/// Validate the traversal arguments, returning the effective hop count
/// (capped at the `pgokf.max_graph_hops` ceiling), the result ceiling, and the
/// normalized relation-type filter (`None` follows every type; an empty array
/// is treated like `NULL`).
#[allow(clippy::type_complexity)]
fn validated_traversal_args(
    max_hops: i32,
    direction: &str,
    relation_types: Option<Vec<String>>,
    max_results: i32,
) -> Result<(i32, usize, Option<Vec<String>>), CatalogError> {
    if !TRAVERSAL_DIRECTIONS.contains(&direction) {
        return Err(CatalogError::invalid_parameter(
            format!(
                "direction must be one of {}, got {direction}",
                TRAVERSAL_DIRECTIONS.map(|d| format!("'{d}'")).join(", ")
            ),
            Path::new(""),
        ));
    }
    if max_hops < 1 {
        return Err(CatalogError::invalid_parameter(
            format!("max_hops must be at least 1, got {max_hops}"),
            Path::new(""),
        ));
    }
    let ceiling = i32::try_from(crate::guc::max_graph_hops()).unwrap_or(i32::MAX);
    let hops = max_hops.min(ceiling.max(1));
    if !(1..=MAX_NEIGHBOR_RESULTS).contains(&max_results) {
        return Err(CatalogError::invalid_parameter(
            format!("max_results must be between 1 and {MAX_NEIGHBOR_RESULTS}, got {max_results}"),
            Path::new(""),
        ));
    }
    let relation_types = match relation_types {
        Some(types) if !types.is_empty() => {
            if types.len() > MAX_RELATION_TYPE_FILTERS {
                return Err(CatalogError::invalid_parameter(
                    format!(
                        "relation_types holds {} entries but the limit is {MAX_RELATION_TYPE_FILTERS}",
                        types.len()
                    ),
                    Path::new(""),
                ));
            }
            for relation_type in &types {
                if relation_type.is_empty() {
                    return Err(CatalogError::invalid_parameter(
                        "relation_types must not contain empty entries",
                        Path::new(""),
                    ));
                }
            }
            Some(types)
        }
        _ => None,
    };
    Ok((
        hops,
        usize::try_from(max_results).unwrap_or(usize::MAX),
        relation_types,
    ))
}

/// The full `concept_relationship_neighbors` read path.
fn relationship_neighbors_impl(
    start_bundle_id: i64,
    start_concept_id: &str,
    max_hops: i32,
    direction: &str,
    relation_types: Option<Vec<String>>,
    max_results: i32,
) -> Result<Vec<TypedNeighborHit>, CatalogError> {
    security::authorize_current_user(security::Operation::Search, Path::new(""))?;
    let (hops, max_results_usize, relation_types) =
        validated_traversal_args(max_hops, direction, relation_types, max_results)?;

    // The seed must exist in an active bundle; otherwise the traversal is
    // empty (mirroring concept_neighbors' unknown-seed behavior).
    let seed_exists = Spi::get_one_with_args::<bool>(
        "SELECT EXISTS (
             SELECT 1 FROM pgokf.concepts c
             JOIN pgokf.bundles b ON b.id = c.bundle_id
                                AND b.enabled AND b.retired_at IS NULL
             WHERE c.bundle_id = $1 AND c.id = $2)",
        &[start_bundle_id.into(), start_concept_id.into()],
    )
    .map_err(|error| spi_error("failed to resolve traversal seed", &error))?
    .unwrap_or(false);
    if !seed_exists {
        return Ok(Vec::new());
    }

    let visits = typed_breadth_first(
        (start_bundle_id, start_concept_id.to_owned()),
        hops,
        max_results_usize,
        |frontier| expand_typed_frontier(frontier, direction, relation_types.as_deref()),
    )?;
    if visits.is_empty() {
        return Ok(Vec::new());
    }

    let nodes: Vec<(i64, String)> = visits.iter().map(|(node, _)| node.clone()).collect();
    let policy = crate::catalog::embedding::effective_embedding_policy()?;
    let annotations = annotate_nodes(&nodes, &policy)?;

    let mut hits: Vec<TypedNeighborHit> = visits
        .into_iter()
        .filter_map(|((bundle_id, concept_id), record)| {
            annotations
                .get(&(bundle_id, concept_id.clone()))
                .map(|annotation| TypedNeighborHit {
                    start_bundle_id,
                    start_concept_id: start_concept_id.to_owned(),
                    bundle_id,
                    concept_id,
                    hops: record.hops,
                    path_bundle_ids: record.path_bundle_ids,
                    path_concept_ids: record.path_concept_ids,
                    relation_type: record.relation_type,
                    title: annotation.title.clone(),
                    freshness_state: annotation.freshness_state.clone(),
                    freshness_reasons: annotation.freshness_reasons.clone(),
                    freshness_scope: annotation.freshness_scope.clone(),
                    stale_since: annotation.stale_since,
                    observed_revision: annotation.observed_revision.clone(),
                    indexed_revision: annotation.indexed_revision.clone(),
                    published_revision: annotation.published_revision.clone(),
                    catalog_generation: annotation.catalog_generation,
                    last_reconciled_at: annotation.last_reconciled_at,
                    embedding_state: annotation.embedding_state.clone(),
                    embedding_model: annotation.embedding_model.clone(),
                    embedding_dim: annotation.embedding_dim,
                    embedding_input_hash: annotation.embedding_input_hash.clone(),
                    embedded_at: annotation.embedded_at,
                })
        })
        .collect();
    hits.sort_by(|left, right| {
        left.hops
            .cmp(&right.hops)
            .then_with(|| left.bundle_id.cmp(&right.bundle_id))
            .then_with(|| left.concept_id.cmp(&right.concept_id))
    });
    hits.truncate(max_results_usize);
    Ok(hits)
}

fn relationship_neighbor_tuple(
    hit: TypedNeighborHit,
) -> Result<PgHeapTuple<'static, AllocatedByRust>, CatalogError> {
    let mut tuple =
        PgHeapTuple::new_composite_type("pgokf.relationship_neighbor").map_err(composite_error)?;
    tuple
        .set_by_name("start_bundle_id", hit.start_bundle_id)
        .map_err(composite_error)?;
    tuple
        .set_by_name("start_concept_id", hit.start_concept_id)
        .map_err(composite_error)?;
    tuple
        .set_by_name("bundle_id", hit.bundle_id)
        .map_err(composite_error)?;
    tuple
        .set_by_name("concept_id", hit.concept_id)
        .map_err(composite_error)?;
    tuple
        .set_by_name("hops", hit.hops)
        .map_err(composite_error)?;
    tuple
        .set_by_name("path_bundle_ids", hit.path_bundle_ids)
        .map_err(composite_error)?;
    tuple
        .set_by_name("path_concept_ids", hit.path_concept_ids)
        .map_err(composite_error)?;
    tuple
        .set_by_name("relation_type", hit.relation_type)
        .map_err(composite_error)?;
    tuple
        .set_by_name("title", hit.title)
        .map_err(composite_error)?;
    tuple
        .set_by_name("freshness_state", hit.freshness_state)
        .map_err(composite_error)?;
    tuple
        .set_by_name("freshness_reasons", hit.freshness_reasons)
        .map_err(composite_error)?;
    tuple
        .set_by_name("freshness_scope", hit.freshness_scope)
        .map_err(composite_error)?;
    tuple
        .set_by_name("stale_since", hit.stale_since)
        .map_err(composite_error)?;
    tuple
        .set_by_name("observed_revision", hit.observed_revision)
        .map_err(composite_error)?;
    tuple
        .set_by_name("indexed_revision", hit.indexed_revision)
        .map_err(composite_error)?;
    tuple
        .set_by_name("published_revision", hit.published_revision)
        .map_err(composite_error)?;
    tuple
        .set_by_name("catalog_generation", hit.catalog_generation)
        .map_err(composite_error)?;
    tuple
        .set_by_name("last_reconciled_at", hit.last_reconciled_at)
        .map_err(composite_error)?;
    tuple
        .set_by_name("embedding_state", hit.embedding_state)
        .map_err(composite_error)?;
    tuple
        .set_by_name("embedding_model", hit.embedding_model)
        .map_err(composite_error)?;
    tuple
        .set_by_name("embedding_dim", hit.embedding_dim)
        .map_err(composite_error)?;
    tuple
        .set_by_name("embedding_input_hash", hit.embedding_input_hash)
        .map_err(composite_error)?;
    tuple
        .set_by_name("embedded_at", hit.embedded_at)
        .map_err(composite_error)?;
    Ok(tuple)
}

/// SQL-facing relationship API, installed into the `pgokf` schema.
#[pgrx::pg_schema]
mod pgokf {
    use pgrx::iter::SetOfIterator;
    use pgrx::{default, extension_sql, pg_extern};

    use super::{
        publication_info_tuple, relationship_neighbor_tuple, relationship_neighbors_impl,
        replace_relationships_impl,
    };

    extension_sql!(
        r"
CREATE TYPE pgokf.relationship_publication_info AS (
    publication_id     bigint,
    tenant_id          text,
    producer           text,
    source_bundle_id   bigint,
    source_bundle_path text,
    publication_generation bigint,
    expected_catalog_generation bigint,
    activated_catalog_generation bigint,
    fencing_token      bigint,
    relationship_set_hash text,
    manifest_hash      text,
    state              text,
    row_count          integer,
    idempotency_key    text,
    created_at         timestamptz,
    activated_at       timestamptz
);

COMMENT ON TYPE pgokf.relationship_publication_info IS
    'One relationship publication as pgokf.replace_relationships reports it: the natural key, the generation/fence binding, the relationship-set hash (doubling as the idempotency key), the state (staged/active/superseded), the row count, and the timestamps.';

CREATE TYPE pgokf.relationship_neighbor AS (
    start_bundle_id    bigint,
    start_concept_id   text,
    bundle_id          bigint,
    concept_id         text,
    hops               integer,
    path_bundle_ids    bigint[],
    path_concept_ids   text[],
    relation_type      text,
    title              text,
    freshness_state    text,
    freshness_reasons  text[],
    freshness_scope    text,
    stale_since        timestamptz,
    observed_revision  text,
    indexed_revision   text,
    published_revision text,
    catalog_generation bigint,
    last_reconciled_at timestamptz,
    embedding_state    text,
    embedding_model    text,
    embedding_dim      integer,
    embedding_input_hash text,
    embedded_at        timestamptz
);

COMMENT ON TYPE pgokf.relationship_neighbor IS
    'One concept reachable from a start concept through pgokf.current_relationships: the (bundle_id, concept_id) node, shortest hop count, the path taken as parallel bundle/concept arrays, the relation type of the reaching edge, the title, and the effective freshness annotation (state, reasons, scope, stale_since, opaque revisions, catalog generation, last_reconciled_at) plus the embedding provenance - the same metadata contract as pgokf.concept_search_fresh.';
",
        name = "relationship_types",
        requires = ["relationship_tables"]
    );

    /// Replace a source bundle's typed relationship set for one publication
    /// generation, atomically.
    ///
    /// Requires membership in `pgokf_writer` (an admin qualifies by
    /// inheritance). `producer` is an opaque label, not authorization. The call
    /// compare-and-sets under the source bundle's advisory lock: `fencing_token`
    /// must be the live, unexpired token of the `(tenant, producer, bundle)`
    /// publication fence and `publication_generation` must equal its target
    /// (SQLSTATE `22023` otherwise). `expected_catalog_generation` equal to the
    /// bundle's current catalog generation activates immediately; equal to the
    /// next generation stages the set (invisible until a refresh accepts exactly
    /// that generation); anything else is rejected. An identical retried call
    /// is a no-op; the same publication key with a different set is a `23505`
    /// conflict. An empty `rows` array removes the prior set on activation.
    #[pg_extern(requires = ["relationship_types"])]
    fn replace_relationships(
        producer: &str,
        source_bundle_id: i64,
        publication_generation: i64,
        expected_catalog_generation: i64,
        fencing_token: i64,
        rows: pgrx::JsonB,
    ) -> pgrx::composite_type!('static, "pgokf.relationship_publication_info") {
        let info = replace_relationships_impl(
            producer,
            source_bundle_id,
            publication_generation,
            expected_catalog_generation,
            fencing_token,
            rows,
        )
        .unwrap_or_else(|error| error.raise());
        publication_info_tuple(&info).unwrap_or_else(|error| error.raise())
    }

    /// Walk the current typed relationships from a start concept.
    ///
    /// Requires membership in `pgokf_reader` (or `pgokf_admin`). Cycle-safe
    /// breadth-first traversal over `pgokf.current_relationships` (only the
    /// active relationship generation, active bundles, tenant-scoped), keyed on
    /// `(bundle_id, concept_id)`, with `direction` `outbound` (the default),
    /// `inbound`, or `both`, an optional `relation_types` filter, `max_hops`
    /// capped at `pgokf.max_graph_hops`, and `max_results` (default 500) capped
    /// at 10000. Unresolved and external rows never become traversal edges.
    /// Every returned node carries its effective freshness annotation and
    /// embedding provenance. An unknown or inactive seed yields an empty
    /// result.
    #[pg_extern(stable, parallel_safe, requires = ["relationship_types", "current_relationships_view"])]
    fn concept_relationship_neighbors(
        start_bundle_id: i64,
        start_concept_id: &str,
        max_hops: default!(i32, 2),
        direction: default!(&str, "'outbound'"),
        relation_types: default!(Option<Vec<String>>, "NULL"),
        max_results: default!(i32, 500),
    ) -> SetOfIterator<'static, pgrx::composite_type!('static, "pgokf.relationship_neighbor")> {
        let hits = relationship_neighbors_impl(
            start_bundle_id,
            start_concept_id,
            max_hops,
            direction,
            relation_types,
            max_results,
        )
        .unwrap_or_else(|error| error.raise());
        let rows: Vec<_> = hits
            .into_iter()
            .map(|hit| relationship_neighbor_tuple(hit).unwrap_or_else(|error| error.raise()))
            .collect();
        SetOfIterator::new(rows)
    }

    extension_sql!(
        r"
ALTER FUNCTION pgokf.replace_relationships(text, bigint, bigint, bigint, bigint, jsonb)
    SECURITY DEFINER SET search_path = pg_catalog, pg_temp;

REVOKE ALL ON FUNCTION pgokf.replace_relationships(text, bigint, bigint, bigint, bigint, jsonb) FROM PUBLIC;
GRANT EXECUTE ON FUNCTION pgokf.replace_relationships(text, bigint, bigint, bigint, bigint, jsonb) TO pgokf_writer;
COMMENT ON FUNCTION pgokf.replace_relationships(text, bigint, bigint, bigint, bigint, jsonb) IS
    'Replace a source bundle''s typed relationship set for one publication generation, atomically, returning pgokf.relationship_publication_info. Writer-tier (pgokf_writer; admin inherits), SECURITY DEFINER, tenant-confined; producer is an opaque label, not authorization. Compare-and-set under the source bundle advisory lock: fencing_token must be the live unexpired token of the (tenant, producer, bundle) publication fence and publication_generation must equal its target (22023 otherwise, so a superseded or expired attempt never publishes). Generation rule, against the bundle''s current catalog generation G: expected = G activates immediately (superseding the producer''s prior active publication); expected = G + 1 stages the set, invisible until a refresh accepts exactly that generation (run_bundle_sync activates it in the sync transaction and supersedes the prior generation''s publications, so new concepts never combine with old-generation relationships); anything else is 22023. rows is a jsonb array of row objects (source_concept_id, namespaced relation_type ''<namespace>:<name>'', optional direction directed|undirected, optional resolved target target_bundle_id + target_concept_id (concept alone targets the source bundle), optional external_target (mutually exclusive with a resolved target), optional source_location/provenance jsonb, optional confidence in [0,1]); at most 10000 rows, duplicate canonical identities are 22023. Endpoint validation never leaks: an absent, inactive, or cross-tenant target bundle resolves to the same unresolved row with the bundle reference dropped. Rows are canonicalized (sorted) and hashed: an identical retried call is a no-op, the same publication key with a different set is 23505. An empty rows array removes the prior set on activation. A bundle whose relationship coverage a refresh supersedes without replacement stays stale (reason relationship_coverage_missing) and pgokf.mark_fresh refuses until a matching replacement activates.';

REVOKE ALL ON FUNCTION pgokf.concept_relationship_neighbors(bigint, text, integer, text, text[], integer) FROM PUBLIC;
GRANT EXECUTE ON FUNCTION pgokf.concept_relationship_neighbors(bigint, text, integer, text, text[], integer) TO pgokf_reader;
COMMENT ON FUNCTION pgokf.concept_relationship_neighbors(bigint, text, integer, text, text[], integer) IS
    'Cycle-safe breadth-first traversal of the current typed relationships from (start_bundle_id, start_concept_id), over pgokf.current_relationships only (the active relationship generation; active bundles; tenant-scoped), keyed on (bundle_id, concept_id). direction is ''outbound'' (default), ''inbound'', or ''both'' (22023 otherwise); relation_types NULL or empty follows every type; max_hops must be at least 1 and is capped at pgokf.max_graph_hops; max_results defaults to 500 and is capped at 10000. Unresolved and external rows never become edges. Each node returns its shortest hop count, path (parallel bundle/concept arrays), the reaching edge''s relation type, title, and the effective freshness annotation plus embedding provenance of pgokf.concept_search_fresh. An unknown or inactive seed yields an empty result. Reader-level, invoker rights. pgokf.concept_neighbors (the Markdown link graph) is unchanged.';
",
        name = "relationship_function_hardening",
        requires = [replace_relationships, concept_relationship_neighbors]
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::errors::ErrorKind;

    /// Build a row object from JSON text (the wire shape `rows` carries).
    fn row_json(text: &str) -> serde_json::Value {
        serde_json::from_str(text).expect("fixture JSON parses")
    }

    #[test]
    fn parse_row_accepts_a_full_wire_row() {
        // Arrange & Act
        let row = parse_row(
            0,
            &row_json(
                r#"{"source_concept_id": "a", "relation_type": "code:calls",
                    "direction": "undirected", "target_bundle_id": 7,
                    "target_concept_id": "b", "confidence": 0.5,
                    "source_location": {"line": 3}, "provenance": {"k": 1}}"#,
            ),
        )
        .expect("a full row is valid");

        // Assert
        assert_eq!(row.source_concept_id, "a");
        assert_eq!(row.relation_type, "code:calls");
        assert_eq!(row.direction, "undirected");
        assert_eq!(row.target_bundle_id, Some(7));
        assert_eq!(row.target_concept_id.as_deref(), Some("b"));
        assert_eq!(row.confidence, Some(0.5));
        assert!(row.source_location.is_some());
        assert!(row.provenance.is_some());
        assert!(!row.unresolved && !row.cross_bundle);
    }

    #[test]
    fn parse_row_defaults_direction_and_allows_bare_targets() {
        // Arrange & Act: a same-bundle target (concept only), an external
        // target, and a target-less row.
        let internal = parse_row(
            0,
            &row_json(
                r#"{"source_concept_id": "a", "relation_type": "ns:rel",
                          "target_concept_id": "b"}"#,
            ),
        )
        .expect("concept-only target is valid");
        let external = parse_row(
            1,
            &row_json(
                r#"{"source_concept_id": "a", "relation_type": "ns:rel",
                          "external_target": "opaque-1"}"#,
            ),
        )
        .expect("external target is valid");
        let bare = parse_row(
            2,
            &row_json(r#"{"source_concept_id": "a", "relation_type": "ns:rel"}"#),
        )
        .expect("a target-less row is valid");

        // Assert
        assert_eq!(internal.direction, "directed");
        assert_eq!(internal.target_bundle_id, None);
        assert!(external.external_target.is_some());
        assert!(bare.target_concept_id.is_none() && bare.external_target.is_none());
    }

    #[test]
    fn parse_row_rejects_shape_violations_with_22023() {
        // Arrange
        let cases = [
            r#"{"relation_type": "ns:rel"}"#, // no source
            r#"{"source_concept_id": "", "relation_type": "ns:rel"}"#, // empty source
            r#"{"source_concept_id": "a"}"#,  // no relation type
            r#"{"source_concept_id": "a", "relation_type": "nonamespace"}"#, // no ':'
            r#"{"source_concept_id": "a", "relation_type": ":rel"}"#, // empty namespace
            r#"{"source_concept_id": "a", "relation_type": "ns:"}"#, // empty name
            r#"{"source_concept_id": "a", "relation_type": "ns:rel", "direction": "sideways"}"#,
            r#"{"source_concept_id": "a", "relation_type": "ns:rel", "target_bundle_id": 3}"#, // bundle without concept
            r#"{"source_concept_id": "a", "relation_type": "ns:rel", "target_bundle_id": -1, "target_concept_id": "b"}"#,
            r#"{"source_concept_id": "a", "relation_type": "ns:rel", "target_concept_id": "b", "external_target": "x"}"#,
            r#"{"source_concept_id": "a", "relation_type": "ns:rel", "confidence": 1.5}"#,
            r#"{"source_concept_id": "a", "relation_type": "ns:rel", "confidence": "high"}"#,
            r#"{"source_concept_id": "a", "relation_type": "ns:rel", "surprise": true}"#, // unknown key
        ];

        // Act / Assert
        for case in cases {
            let error = parse_row(0, &row_json(case)).expect_err("must be rejected");
            assert_eq!(error.kind(), ErrorKind::InvalidParameter, "{case}");
            assert_eq!(error.sqlstate(), "22023", "{case}");
        }
    }

    #[test]
    fn parse_rows_requires_an_array_within_the_hard_limit() {
        // Arrange & Act & Assert
        assert!(parse_rows(&JsonB(row_json(r#"{"not": "an array"}"#))).is_err());
        assert!(
            parse_rows(&JsonB(row_json("[]")))
                .expect("empty is valid")
                .is_empty()
        );
    }

    #[test]
    fn reject_duplicates_catches_repeated_canonical_identities() {
        // Arrange: two rows equal on the canonical identity but differing in
        // opaque metadata.
        let first = parse_row(
            0,
            &row_json(
                r#"{"source_concept_id": "a", "relation_type": "ns:rel",
                          "target_concept_id": "b", "confidence": 0.1}"#,
            ),
        )
        .expect("first row is valid");
        let second = parse_row(
            1,
            &row_json(
                r#"{"source_concept_id": "a", "relation_type": "ns:rel",
                          "target_concept_id": "b", "confidence": 0.9}"#,
            ),
        )
        .expect("second row is valid");

        // Act / Assert
        assert!(reject_duplicates(&[first]).is_ok());
        // Two rows differing only in opaque metadata share one canonical
        // identity and are rejected.
        let error = reject_duplicates(&[second.clone(), second]).expect_err("dup is rejected");
        assert_eq!(error.kind(), ErrorKind::InvalidParameter);
        let error = reject_duplicates(&[
            parse_row(
                0,
                &row_json(r#"{"source_concept_id": "a", "relation_type": "ns:rel", "target_concept_id": "b"}"#),
            )
            .expect("row is valid"),
            parse_row(
                1,
                &row_json(r#"{"source_concept_id": "a", "relation_type": "ns:rel", "target_concept_id": "b"}"#),
            )
            .expect("row is valid"),
        ])
        .expect_err("a duplicate canonical identity is rejected");
        assert_eq!(error.kind(), ErrorKind::InvalidParameter);
    }

    #[test]
    fn canonicalize_is_order_independent_and_stable_for_retries() {
        // Arrange: the same three rows in two different orders.
        let one =
            r#"{"source_concept_id": "a", "relation_type": "ns:x", "target_concept_id": "b"}"#;
        let two = r#"{"source_concept_id": "a", "relation_type": "ns:y"}"#;
        let three =
            r#"{"source_concept_id": "c", "relation_type": "ns:x", "external_target": "e"}"#;
        let forward = vec![
            parse_row(0, &row_json(one)).expect("row parses"),
            parse_row(1, &row_json(two)).expect("row parses"),
            parse_row(2, &row_json(three)).expect("row parses"),
        ];
        let reverse: Vec<_> = forward.iter().rev().cloned().collect();

        // Act
        let (forward_rows, forward_hash) = canonicalize(forward);
        let (reverse_rows, reverse_hash) = canonicalize(reverse);

        // Assert: identical sets hash identically regardless of submission
        // order, and the canonical order is sorted by identity.
        assert_eq!(forward_hash, reverse_hash);
        assert_eq!(forward_rows, reverse_rows);
        let identities: Vec<CanonicalIdentity<'_>> = forward_rows
            .iter()
            .map(|(row, _)| canonical_identity(row))
            .collect();
        let mut sorted = identities.clone();
        sorted.sort();
        assert_eq!(identities, sorted);
        // The empty set has one fixed digest (the hash of the empty input).
        let (_, empty_hash) = canonicalize(Vec::new());
        assert_ne!(empty_hash, forward_hash);
    }

    #[test]
    fn typed_breadth_first_is_cycle_safe_across_bundles() {
        // Arrange: a cycle crossing two bundles, (1,a) -> (2,b) -> (1,a), plus
        // a forward chain (2,b) -> (2,c).
        let edge = |fb: i64, fc: &str, tb: i64, tc: &str| TypedEdge {
            from: (fb, fc.to_owned()),
            to: (tb, tc.to_owned()),
            relation_type: "ns:rel".to_owned(),
        };
        let graph = [
            edge(1, "a", 2, "b"),
            edge(2, "b", 1, "a"),
            edge(2, "b", 2, "c"),
        ];
        let edges_from = |frontier: &[(i64, String)]| {
            Ok(graph
                .iter()
                .filter(|edge| frontier.contains(&edge.from))
                .map(|edge| TypedEdge {
                    from: edge.from.clone(),
                    to: edge.to.clone(),
                    relation_type: edge.relation_type.clone(),
                })
                .collect::<Vec<_>>())
        };

        // Act
        let visits =
            typed_breadth_first((1, "a".to_owned()), 5, 100, edges_from).expect("BFS runs");

        // Assert: the seed is never re-expanded across the cycle; (2,b) is at
        // hop 1, (2,c) at hop 2, and the (1,a) node appears only as the seed.
        let nodes: Vec<(i64, String)> = visits.iter().map(|(node, _)| node.clone()).collect();
        assert_eq!(nodes, vec![(2, "b".to_owned()), (2, "c".to_owned())]);
        let hops: Vec<i32> = visits.iter().map(|(_, record)| record.hops).collect();
        assert_eq!(hops, vec![1, 2]);
        let (_, record) = &visits[1];
        assert_eq!(record.path_bundle_ids, vec![1, 2, 2]);
        assert_eq!(record.path_concept_ids, vec!["a", "b", "c"]);
    }

    #[test]
    fn typed_breadth_first_honors_the_result_ceiling() {
        // Arrange: a star with more leaves than the ceiling.
        let graph: Vec<TypedEdge> = (0..5)
            .map(|leaf| TypedEdge {
                from: (1, "root".to_owned()),
                to: (1, format!("leaf{leaf}")),
                relation_type: "ns:rel".to_owned(),
            })
            .collect();
        let edges_from = |frontier: &[(i64, String)]| {
            Ok(graph
                .iter()
                .filter(|edge| frontier.contains(&edge.from))
                .map(|edge| TypedEdge {
                    from: edge.from.clone(),
                    to: edge.to.clone(),
                    relation_type: edge.relation_type.clone(),
                })
                .collect::<Vec<_>>())
        };

        // Act
        let visits =
            typed_breadth_first((1, "root".to_owned()), 3, 2, edges_from).expect("BFS runs");

        // Assert: discovery stops at the ceiling.
        assert_eq!(visits.len(), 2);
    }
}
