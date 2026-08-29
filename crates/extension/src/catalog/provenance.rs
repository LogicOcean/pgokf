// SPDX-License-Identifier: AGPL-3.0-only
//! OKF v0.2 provenance / trust / lifecycle projection seam.
//!
//! # Seam contract for the provenance feature wave
//!
//! Everything lives in **this file only** - the sync engine already calls
//! [`project`] inside the advisory-locked atomic transaction and must not be
//! edited. This wave owns three tables, all keyed on
//! `(bundle_id, concept_id) REFERENCES pgokf.concepts (bundle_id, id)
//! ON DELETE CASCADE` so a removed concept drops its provenance automatically
//! (which is why removals need no seam call):
//!
//! 1. `pgokf.concept_provenance` - one scalar generation/trust/lifecycle row
//!    per provenance-bearing concept.
//! 2. `pgokf.concept_verification` - the ordered `verified[]` event list.
//! 3. `pgokf.concept_provenance_source` - the `sources[]` provenance
//!    materials (distinct from the raw-bytes `pgokf.concept_source`).
//!
//! # OKF v0.2 field mapping
//!
//! The typed columns are derived defensively from the frontmatter the core
//! parser leaves unmodeled in [`okf_parser::ParsedConcept::metadata`]; every
//! value may be absent, wrong-typed, or nested, and any such case is coerced or
//! skipped, never panicked on, and never aborts the surrounding sync.
//!
//! | Column                                | OKF v0.2 source                                             |
//! | ------------------------------------- | ----------------------------------------------------------- |
//! | `concept_provenance.generated_by`     | `generated.by` (tolerates a bare `generated_by`)            |
//! | `concept_provenance.generated_at`     | `generated.at` (ISO 8601, tolerates a bare `generated_at`)  |
//! | `concept_provenance.status`           | `status` (LIFECYCLE; spec default when absent is `stable`)  |
//! | `concept_provenance.stale_after`      | `stale_after` (ISO 8601 absolute instant)                  |
//! | `concept_provenance.usage_window_*`   | top-level `usage_window {from,to}`                          |
//! | `concept_provenance.trust_tier`       | DERIVED from the `verified[]` actors (see [`trust_tier`])   |
//! | `concept_verification.*`              | each `verified[]` event `{by,at}`                          |
//! | `concept_provenance_source.*`         | each `sources[]` entry `{id,resource,title,author,…}`      |
//!
//! # Trust-tier derivation
//!
//! `verified` is a LIST of events; a single mapping is treated as a
//! one-element list. The derived tier is `unverified` with no events,
//! `machine-confirmed` with at least one event whose actor is non-human, and
//! `human-reviewed` as soon as any event actor is a `human:` actor.
//!
//! # Timestamp handling
//!
//! ISO 8601 instants are parsed to Unix-epoch seconds entirely in Rust
//! ([`parse_iso8601_epoch`]) and converted to `timestamptz` in SQL with
//! `to_timestamp`, exactly as the sync engine already handles filesystem
//! modification times. A malformed or calendar-invalid instant yields `None`
//! → SQL `NULL`; the raw string is still retained losslessly in `details`.
//! Parsing in Rust means no cast can ever throw and abort the sync.
//!
//! # Row semantics
//!
//! The projection is **sparse**: a concept carrying no recognized
//! provenance/trust/lifecycle key produces no `concept_provenance` row and no
//! child rows, so a `LEFT JOIN` distinguishes "no provenance frontmatter" from
//! "provenance present but unverified". Projection is delete-then-insert per
//! staged concept across all three tables, so re-syncing a concept whose
//! provenance frontmatter was removed correctly drops its stale rows.

use std::path::Path;

use pgrx::{Spi, extension_sql};

use crate::catalog::batch::BATCH_SIZE;
use crate::catalog::iso8601::parse_iso8601_epoch;
use crate::catalog::types::{StagedConcept, count_to_i32};
use crate::errors::CatalogError;
use okf_parser::{ParsedConcept, Value};

extension_sql!(
    r"
CREATE TABLE pgokf.concept_provenance (
    bundle_id         bigint NOT NULL,
    concept_id        text   NOT NULL,
    generated_by      text,
    generated_at      timestamptz,
    status            text,
    stale_after       timestamptz,
    usage_window_from timestamptz,
    usage_window_to   timestamptz,
    trust_tier        text,
    details           jsonb  NOT NULL DEFAULT '{}'::jsonb,
    tenant_id         text   NOT NULL DEFAULT 'default',
    CONSTRAINT concept_provenance_pkey PRIMARY KEY (bundle_id, concept_id),
    CONSTRAINT concept_provenance_concept_fk
        FOREIGN KEY (bundle_id, concept_id)
        REFERENCES pgokf.concepts (bundle_id, id)
        ON DELETE CASCADE
);

CREATE TABLE pgokf.concept_verification (
    bundle_id   bigint  NOT NULL,
    concept_id  text    NOT NULL,
    ordinal     integer NOT NULL,
    verified_by text    NOT NULL,
    verified_at timestamptz,
    tenant_id   text    NOT NULL DEFAULT 'default',
    CONSTRAINT concept_verification_pkey PRIMARY KEY (bundle_id, concept_id, ordinal),
    CONSTRAINT concept_verification_concept_fk
        FOREIGN KEY (bundle_id, concept_id)
        REFERENCES pgokf.concepts (bundle_id, id)
        ON DELETE CASCADE
);

CREATE TABLE pgokf.concept_provenance_source (
    bundle_id         bigint  NOT NULL,
    concept_id        text    NOT NULL,
    ordinal           integer NOT NULL,
    source_id         text,
    resource          text,
    title             text,
    author            text,
    usage_count       bigint,
    last_modified     timestamptz,
    usage_window_from timestamptz,
    usage_window_to   timestamptz,
    tenant_id         text    NOT NULL DEFAULT 'default',
    CONSTRAINT concept_provenance_source_pkey PRIMARY KEY (bundle_id, concept_id, ordinal),
    CONSTRAINT concept_provenance_source_concept_fk
        FOREIGN KEY (bundle_id, concept_id)
        REFERENCES pgokf.concepts (bundle_id, id)
        ON DELETE CASCADE
);

CREATE INDEX concept_provenance_trust_tier_idx
    ON pgokf.concept_provenance (trust_tier);

-- Multi-tenant isolation (see pgokf.bundles): opt-in-by-usage RLS on the
-- denormalized tenant_id of each provenance table. Not forced, so the SECURITY
-- DEFINER sync path bypasses it to project a single-tenant bundle's rows.
ALTER TABLE pgokf.concept_provenance ENABLE ROW LEVEL SECURITY;
CREATE POLICY concept_provenance_tenant_isolation ON pgokf.concept_provenance
    USING (pg_catalog.current_setting('pgokf.tenant', true) IS NULL
        OR pg_catalog.current_setting('pgokf.tenant', true) = ''
        OR tenant_id = pg_catalog.current_setting('pgokf.tenant', true))
    WITH CHECK (pg_catalog.current_setting('pgokf.tenant', true) IS NULL
        OR pg_catalog.current_setting('pgokf.tenant', true) = ''
        OR tenant_id = pg_catalog.current_setting('pgokf.tenant', true));

ALTER TABLE pgokf.concept_verification ENABLE ROW LEVEL SECURITY;
CREATE POLICY concept_verification_tenant_isolation ON pgokf.concept_verification
    USING (pg_catalog.current_setting('pgokf.tenant', true) IS NULL
        OR pg_catalog.current_setting('pgokf.tenant', true) = ''
        OR tenant_id = pg_catalog.current_setting('pgokf.tenant', true))
    WITH CHECK (pg_catalog.current_setting('pgokf.tenant', true) IS NULL
        OR pg_catalog.current_setting('pgokf.tenant', true) = ''
        OR tenant_id = pg_catalog.current_setting('pgokf.tenant', true));

ALTER TABLE pgokf.concept_provenance_source ENABLE ROW LEVEL SECURITY;
CREATE POLICY concept_provenance_source_tenant_isolation ON pgokf.concept_provenance_source
    USING (pg_catalog.current_setting('pgokf.tenant', true) IS NULL
        OR pg_catalog.current_setting('pgokf.tenant', true) = ''
        OR tenant_id = pg_catalog.current_setting('pgokf.tenant', true))
    WITH CHECK (pg_catalog.current_setting('pgokf.tenant', true) IS NULL
        OR pg_catalog.current_setting('pgokf.tenant', true) = ''
        OR tenant_id = pg_catalog.current_setting('pgokf.tenant', true));

COMMENT ON TABLE pgokf.concept_provenance IS
    'Scalar OKF v0.2 generation/trust/lifecycle projection: one row per concept that carries any provenance, trust, or lifecycle frontmatter (sparse). The verified[] event list and the sources[] materials live in pgokf.concept_verification and pgokf.concept_provenance_source; the full lossless key subset is in details.';
COMMENT ON COLUMN pgokf.concept_provenance.generated_by IS
    'OKF generated.by: the actor (agent/human/process) that produced the current content; tolerates a bare generated_by. NULL when absent or not a string.';
COMMENT ON COLUMN pgokf.concept_provenance.generated_at IS
    'OKF generated.at: when the current content was produced, parsed from ISO 8601; tolerates a bare generated_at. NULL when absent or unparseable (the raw value stays in details).';
COMMENT ON COLUMN pgokf.concept_provenance.status IS
    'OKF lifecycle status (draft|stable|deprecated). NULL when absent; the OKF v0.2 spec default for an absent status is stable.';
COMMENT ON COLUMN pgokf.concept_provenance.stale_after IS
    'OKF stale_after: the absolute ISO 8601 instant after which the content is considered stale. NULL when absent or unparseable.';
COMMENT ON COLUMN pgokf.concept_provenance.usage_window_from IS
    'OKF top-level usage_window.from: start of the window framing all source usage_counts. NULL when absent or unparseable.';
COMMENT ON COLUMN pgokf.concept_provenance.usage_window_to IS
    'OKF top-level usage_window.to: end of the window framing all source usage_counts. NULL when absent or unparseable.';
COMMENT ON COLUMN pgokf.concept_provenance.trust_tier IS
    'Derived OKF trust tier: human-reviewed when any verified[] actor is a human:, else machine-confirmed with >=1 verified event, else unverified.';
COMMENT ON COLUMN pgokf.concept_provenance.details IS
    'Lossless jsonb copy of the recognized OKF provenance/trust/lifecycle key subset (generated, verified, sources, usage_window, stale_after, status, and the generated_by alias).';
COMMENT ON COLUMN pgokf.concept_provenance.tenant_id IS
    'Multi-tenant owner, denormalized from the concept''s bundle for a local row-level-security predicate; always equals the bundle''s tenant_id.';

COMMENT ON TABLE pgokf.concept_verification IS
    'One row per OKF v0.2 verified[] event for a concept: the ordered list of verification events (a single mapping is stored as one 0-ordinal row). Cascades from pgokf.concepts.';
COMMENT ON COLUMN pgokf.concept_verification.ordinal IS
    'Zero-based position of the event in the concept''s verified[] list; forms the primary key with (bundle_id, concept_id).';
COMMENT ON COLUMN pgokf.concept_verification.verified_by IS
    'OKF verified[].by: the actor that performed the verification (agent/human:/process:). Events with no actor are skipped, never stored as NULL.';
COMMENT ON COLUMN pgokf.concept_verification.verified_at IS
    'OKF verified[].at, parsed from ISO 8601. NULL when the at value is absent or unparseable.';
COMMENT ON COLUMN pgokf.concept_verification.tenant_id IS
    'Multi-tenant owner, denormalized from the concept''s bundle for a local row-level-security predicate; always equals the bundle''s tenant_id.';

COMMENT ON TABLE pgokf.concept_provenance_source IS
    'One row per OKF v0.2 sources[] provenance material for a concept - the inputs the content was derived from. Distinct from pgokf.concept_source, which holds the concept''s own raw source bytes. Cascades from pgokf.concepts.';
COMMENT ON COLUMN pgokf.concept_provenance_source.ordinal IS
    'Zero-based position of the entry in the concept''s sources[] list; forms the primary key with (bundle_id, concept_id).';
COMMENT ON COLUMN pgokf.concept_provenance_source.source_id IS
    'OKF sources[].id: an optional producer-defined identifier for the source. NULL when absent.';
COMMENT ON COLUMN pgokf.concept_provenance_source.resource IS
    'OKF sources[].resource: the source URI. Spec-required per entry but stored leniently (NULL when absent) so a malformed source never aborts the sync.';
COMMENT ON COLUMN pgokf.concept_provenance_source.title IS
    'OKF sources[].title: an optional human-readable title for the source.';
COMMENT ON COLUMN pgokf.concept_provenance_source.author IS
    'OKF sources[].author: the actor credited with the source.';
COMMENT ON COLUMN pgokf.concept_provenance_source.usage_count IS
    'OKF sources[].usage_count: how many times the source was used within the usage_window. NULL when absent or non-numeric.';
COMMENT ON COLUMN pgokf.concept_provenance_source.last_modified IS
    'OKF sources[].last_modified, parsed from ISO 8601. NULL when absent or unparseable.';
COMMENT ON COLUMN pgokf.concept_provenance_source.usage_window_from IS
    'OKF sources[].usage_window.from: start of this source''s own usage window, overriding the top-level window. NULL when absent or unparseable.';
COMMENT ON COLUMN pgokf.concept_provenance_source.usage_window_to IS
    'OKF sources[].usage_window.to: end of this source''s own usage window. NULL when absent or unparseable.';
COMMENT ON COLUMN pgokf.concept_provenance_source.tenant_id IS
    'Multi-tenant owner, denormalized from the concept''s bundle for a local row-level-security predicate; always equals the bundle''s tenant_id.';

GRANT SELECT ON pgokf.concept_provenance TO pgokf_reader;
GRANT SELECT ON pgokf.concept_verification TO pgokf_reader;
GRANT SELECT ON pgokf.concept_provenance_source TO pgokf_reader;
",
    name = "provenance_table",
    requires = ["catalog_tables"]
);

/// OKF v0.2 frontmatter keys treated as provenance/trust/lifecycle data and
/// retained verbatim in `concept_provenance.details`.
///
/// The set mirrors the OKF v0.2 PROVENANCE, TRUST, and LIFECYCLE families
/// (origin, verification, usage window, lifecycle status, and declared
/// sources) plus the tolerated `generated_by` alias. Type-specific keys (`runtime`,
/// `parameters`, `computation`, …) are not provenance data and stay in
/// `pgokf.concept_metadata`, so restricting the subset never loses data.
const PROVENANCE_KEYS: &[&str] = &[
    "generated",
    "generated_at",
    "generated_by",
    "verified",
    "sources",
    "usage_window",
    "stale_after",
    "status",
];

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

/// Read `object[key]` as a string, tolerating a non-object `value` (yields
/// `None`). `serde_json::Value::get` returns `None` for any non-object, so this
/// is safe on scalars and arrays alike.
fn json_str<'value>(value: &'value Value, key: &str) -> Option<&'value str> {
    value.get(key).and_then(Value::as_str)
}

/// Coerce a JSON value to `i64` across the numeric shapes producers ship
/// (signed, unsigned, or a numeric string). Non-numeric values yield `None`.
fn coerce_i64(value: &Value) -> Option<i64> {
    if let Some(number) = value.as_i64() {
        return Some(number);
    }
    if let Some(number) = value.as_u64() {
        return i64::try_from(number).ok();
    }
    value.as_str().and_then(|text| text.trim().parse().ok())
}

/// One verification event extracted from the `verified[]` list.
#[derive(Debug, Clone, PartialEq)]
struct VerificationEvent {
    ordinal: i32,
    verified_by: String,
    verified_at: Option<f64>,
}

/// Extract the ordered `verified[]` events, tolerating a single mapping as a
/// one-element list and skipping any event that carries no actor (the
/// `verified_by` column is `NOT NULL`). The `ordinal` is the event's position
/// in the source list, so a skipped malformed entry leaves a gap rather than
/// renumbering its peers.
///
/// The JSON values reach us only through `pgrx::JsonB` / `okf_parser`, so this
/// crate never names `serde_json`; every field is read through the inherent
/// [`Value::get`](https://docs.rs/serde_json)/`as_*` accessors instead.
fn extract_verification_events(concept: &ParsedConcept) -> Vec<VerificationEvent> {
    let Some(value) = concept.metadata.get("verified") else {
        return Vec::new();
    };

    let mut events = Vec::new();
    let mut consider = |index: usize, item: &_| {
        let Some(verified_by) = json_str(item, "by").and_then(non_empty) else {
            return;
        };
        events.push(VerificationEvent {
            ordinal: count_to_i32(index),
            verified_by,
            verified_at: json_str(item, "at").and_then(parse_iso8601_epoch),
        });
    };

    if let Some(array) = value.as_array() {
        for (index, item) in array.iter().enumerate() {
            consider(index, item);
        }
    } else if value.is_object() {
        // OKF v0.2: a single verified mapping is a one-element list.
        consider(0, value);
    }
    events
}

/// Derive the OKF trust tier from a concept's recorded verification events.
///
/// `human-reviewed` as soon as any event actor is a `human:` actor, else
/// `machine-confirmed` with at least one event, else `unverified`.
fn trust_tier(events: &[VerificationEvent]) -> &'static str {
    if events
        .iter()
        .any(|event| event.verified_by.starts_with("human:"))
    {
        "human-reviewed"
    } else if events.is_empty() {
        "unverified"
    } else {
        "machine-confirmed"
    }
}

/// One provenance-source entry extracted from the `sources[]` list.
#[derive(Debug, Clone, PartialEq)]
struct ProvenanceSourceEntry {
    ordinal: i32,
    source_id: Option<String>,
    resource: Option<String>,
    title: Option<String>,
    author: Option<String>,
    usage_count: Option<i64>,
    last_modified: Option<f64>,
    usage_window_from: Option<f64>,
    usage_window_to: Option<f64>,
}

/// Extract the ordered `sources[]` provenance materials, skipping any entry
/// that is not an object. The `ordinal` is the entry's position in the source
/// list.
fn extract_provenance_sources(concept: &ParsedConcept) -> Vec<ProvenanceSourceEntry> {
    let Some(array) = concept.metadata.get("sources").and_then(Value::as_array) else {
        return Vec::new();
    };

    let mut sources = Vec::new();
    for (index, item) in array.iter().enumerate() {
        if !item.is_object() {
            continue;
        }
        let window = item.get("usage_window");
        let window_bound = |bound: &str| {
            window
                .and_then(|window| json_str(window, bound))
                .and_then(parse_iso8601_epoch)
        };
        sources.push(ProvenanceSourceEntry {
            ordinal: count_to_i32(index),
            source_id: json_str(item, "id").and_then(non_empty),
            resource: json_str(item, "resource").and_then(non_empty),
            title: json_str(item, "title").and_then(non_empty),
            author: json_str(item, "author").and_then(non_empty),
            usage_count: item.get("usage_count").and_then(coerce_i64),
            last_modified: json_str(item, "last_modified").and_then(parse_iso8601_epoch),
            usage_window_from: window_bound("from"),
            usage_window_to: window_bound("to"),
        });
    }
    sources
}

/// The scalar generation/trust/lifecycle columns of one concept.
#[derive(Debug, Clone, Default, PartialEq)]
struct ScalarProvenance {
    generated_by: Option<String>,
    generated_at: Option<f64>,
    status: Option<String>,
    stale_after: Option<f64>,
    usage_window_from: Option<f64>,
    usage_window_to: Option<f64>,
    trust_tier: String,
}

/// Resolve `generated.by`, tolerating a bare scalar `generated_by` or a bare
/// string `generated`.
fn extract_generated_by(concept: &ParsedConcept) -> Option<String> {
    let metadata = &concept.metadata;
    if let Some(text) = metadata.get("generated_by").and_then(Value::as_str) {
        return non_empty(text);
    }
    let generated = metadata.get("generated")?;
    if let Some(text) = generated.as_str() {
        return non_empty(text);
    }
    json_str(generated, "by").and_then(non_empty)
}

/// Resolve `generated.at`, tolerating a bare scalar `generated_at`.
fn extract_generated_at(concept: &ParsedConcept) -> Option<f64> {
    let metadata = &concept.metadata;
    if let Some(epoch) = metadata
        .get("generated_at")
        .and_then(Value::as_str)
        .and_then(parse_iso8601_epoch)
    {
        return Some(epoch);
    }
    metadata
        .get("generated")
        .and_then(|generated| json_str(generated, "at"))
        .and_then(parse_iso8601_epoch)
}

/// Extract the scalar provenance columns, given the concept's already-parsed
/// verification events (from which the trust tier is derived).
fn extract_scalar(concept: &ParsedConcept, events: &[VerificationEvent]) -> ScalarProvenance {
    let metadata = &concept.metadata;
    let window = metadata.get("usage_window");
    let window_bound = |bound: &str| {
        window
            .and_then(|window| json_str(window, bound))
            .and_then(parse_iso8601_epoch)
    };
    ScalarProvenance {
        generated_by: extract_generated_by(concept),
        generated_at: extract_generated_at(concept),
        status: metadata
            .get("status")
            .and_then(Value::as_str)
            .and_then(non_empty),
        stale_after: metadata
            .get("stale_after")
            .and_then(Value::as_str)
            .and_then(parse_iso8601_epoch),
        usage_window_from: window_bound("from"),
        usage_window_to: window_bound("to"),
        trust_tier: trust_tier(events).to_owned(),
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

/// Every provenance table's rows for a bundle, transposed into the parallel
/// arrays bound by the bulk array-unnest `INSERT`s. Each group of `Vec`s shares
/// length and ordering: row `i` of a group is one row of that table.
#[derive(Debug, Clone, Default)]
struct ProvenanceRows {
    // pgokf.concept_provenance (scalar; one row per provenance-bearing concept)
    scalar_concept_ids: Vec<String>,
    generated_by: Vec<Option<String>>,
    generated_at: Vec<Option<f64>>,
    status: Vec<Option<String>>,
    stale_after: Vec<Option<f64>>,
    usage_window_from: Vec<Option<f64>>,
    usage_window_to: Vec<Option<f64>>,
    trust_tier: Vec<String>,
    details: Vec<String>,
    // pgokf.concept_verification (one row per verified[] event)
    verification_concept_ids: Vec<String>,
    verification_ordinals: Vec<i32>,
    verified_by: Vec<String>,
    verified_at: Vec<Option<f64>>,
    // pgokf.concept_provenance_source (one row per sources[] entry)
    source_concept_ids: Vec<String>,
    source_ordinals: Vec<i32>,
    source_ids: Vec<Option<String>>,
    source_resources: Vec<Option<String>>,
    source_titles: Vec<Option<String>>,
    source_authors: Vec<Option<String>>,
    source_usage_counts: Vec<Option<i64>>,
    source_last_modified: Vec<Option<f64>>,
    source_usage_window_from: Vec<Option<f64>>,
    source_usage_window_to: Vec<Option<f64>>,
}

/// Collect every provenance table's rows for the staged concepts, in staging
/// order, skipping concepts that carry no recognized provenance frontmatter
/// (the projection is sparse).
fn collect_provenance_rows(staged: &[StagedConcept]) -> ProvenanceRows {
    let mut rows = ProvenanceRows::default();
    for entry in staged {
        let concept = &entry.concept;
        let details = extract_details(concept);
        if details_is_empty(&details) {
            continue;
        }

        let events = extract_verification_events(concept);
        let scalar = extract_scalar(concept, &events);

        rows.scalar_concept_ids.push(concept.id.clone());
        rows.generated_by.push(scalar.generated_by);
        rows.generated_at.push(scalar.generated_at);
        rows.status.push(scalar.status);
        rows.stale_after.push(scalar.stale_after);
        rows.usage_window_from.push(scalar.usage_window_from);
        rows.usage_window_to.push(scalar.usage_window_to);
        rows.trust_tier.push(scalar.trust_tier);
        // Compact JSON text of the lossless payload; `serde_json::Value`'s
        // `Display` matches what `pgrx::JsonB` serializes before `jsonb_in`, so
        // the `::jsonb` cast below reproduces a row-by-row `JsonB` binding.
        rows.details.push(details.0.to_string());

        for event in events {
            rows.verification_concept_ids.push(concept.id.clone());
            rows.verification_ordinals.push(event.ordinal);
            rows.verified_by.push(event.verified_by);
            rows.verified_at.push(event.verified_at);
        }

        for source in extract_provenance_sources(concept) {
            rows.source_concept_ids.push(concept.id.clone());
            rows.source_ordinals.push(source.ordinal);
            rows.source_ids.push(source.source_id);
            rows.source_resources.push(source.resource);
            rows.source_titles.push(source.title);
            rows.source_authors.push(source.author);
            rows.source_usage_counts.push(source.usage_count);
            rows.source_last_modified.push(source.last_modified);
            rows.source_usage_window_from.push(source.usage_window_from);
            rows.source_usage_window_to.push(source.usage_window_to);
        }
    }
    rows
}

/// Statements that clear one staged concept's rows from each provenance table,
/// so re-projection is idempotent and stale rows never linger.
const DELETE_STATEMENTS: [&str; 3] = [
    "DELETE FROM pgokf.concept_provenance WHERE bundle_id = $1 AND concept_id = ANY($2)",
    "DELETE FROM pgokf.concept_verification WHERE bundle_id = $1 AND concept_id = ANY($2)",
    "DELETE FROM pgokf.concept_provenance_source WHERE bundle_id = $1 AND concept_id = ANY($2)",
];

/// Clear every staged concept's existing provenance rows, in bounded batches.
///
/// Every staged concept is cleared across all three tables - including ones that
/// now carry no provenance frontmatter and thus contribute no replacement row -
/// so removing provenance from a concept correctly drops its stale rows. Concept
/// IDs are chunked at [`BATCH_SIZE`] so the `= ANY($2)` list never grows
/// unbounded.
fn delete_staged_provenance(bundle_id: i64, staged: &[StagedConcept]) -> Result<(), CatalogError> {
    for chunk in staged.chunks(BATCH_SIZE) {
        let concept_ids: Vec<&str> = chunk
            .iter()
            .map(|entry| entry.concept.id.as_str())
            .collect();
        for statement in DELETE_STATEMENTS {
            Spi::run_with_args(statement, &[bundle_id.into(), concept_ids.clone().into()])
                .map_err(|error| spi_error("failed to clear concept provenance", &error))?;
        }
    }
    Ok(())
}

/// Bulk-insert the scalar `concept_provenance` rows, one array-unnest `INSERT`
/// per [`BATCH_SIZE`] chunk. Epoch-second timestamps are converted to
/// `timestamptz` with `to_timestamp` (a `NULL` epoch yields a `NULL` instant).
fn insert_scalar_rows(bundle_id: i64, rows: &ProvenanceRows) -> Result<(), CatalogError> {
    const INSERT: &str = "
        INSERT INTO pgokf.concept_provenance
            (bundle_id, tenant_id, concept_id, generated_by, generated_at, status,
             stale_after, usage_window_from, usage_window_to, trust_tier, details)
        SELECT
            $1,
            (SELECT b.tenant_id FROM pgokf.bundles b WHERE b.id = $1),
            d.concept_id, d.generated_by,
            pg_catalog.to_timestamp(d.generated_at), d.status,
            pg_catalog.to_timestamp(d.stale_after),
            pg_catalog.to_timestamp(d.usage_window_from),
            pg_catalog.to_timestamp(d.usage_window_to),
            d.trust_tier, d.details::jsonb
        FROM unnest(
                 $2::text[], $3::text[], $4::float8[], $5::text[], $6::float8[],
                 $7::float8[], $8::float8[], $9::text[], $10::text[])
             AS d(concept_id, generated_by, generated_at, status, stale_after,
                   usage_window_from, usage_window_to, trust_tier, details)";

    let total = rows.scalar_concept_ids.len();
    for start in (0..total).step_by(BATCH_SIZE) {
        let end = usize::min(start + BATCH_SIZE, total);
        Spi::run_with_args(
            INSERT,
            &[
                bundle_id.into(),
                rows.scalar_concept_ids[start..end].to_vec().into(),
                rows.generated_by[start..end].to_vec().into(),
                rows.generated_at[start..end].to_vec().into(),
                rows.status[start..end].to_vec().into(),
                rows.stale_after[start..end].to_vec().into(),
                rows.usage_window_from[start..end].to_vec().into(),
                rows.usage_window_to[start..end].to_vec().into(),
                rows.trust_tier[start..end].to_vec().into(),
                rows.details[start..end].to_vec().into(),
            ],
        )
        .map_err(|error| spi_error("failed to insert concept provenance", &error))?;
    }
    Ok(())
}

/// Bulk-insert the `concept_verification` event rows, one array-unnest `INSERT`
/// per [`BATCH_SIZE`] chunk.
fn insert_verification_rows(bundle_id: i64, rows: &ProvenanceRows) -> Result<(), CatalogError> {
    const INSERT: &str = "
        INSERT INTO pgokf.concept_verification
            (bundle_id, tenant_id, concept_id, ordinal, verified_by, verified_at)
        SELECT
            $1,
            (SELECT b.tenant_id FROM pgokf.bundles b WHERE b.id = $1),
            d.concept_id, d.ordinal, d.verified_by,
            pg_catalog.to_timestamp(d.verified_at)
        FROM unnest($2::text[], $3::integer[], $4::text[], $5::float8[])
             AS d(concept_id, ordinal, verified_by, verified_at)";

    let total = rows.verification_concept_ids.len();
    for start in (0..total).step_by(BATCH_SIZE) {
        let end = usize::min(start + BATCH_SIZE, total);
        Spi::run_with_args(
            INSERT,
            &[
                bundle_id.into(),
                rows.verification_concept_ids[start..end].to_vec().into(),
                rows.verification_ordinals[start..end].to_vec().into(),
                rows.verified_by[start..end].to_vec().into(),
                rows.verified_at[start..end].to_vec().into(),
            ],
        )
        .map_err(|error| spi_error("failed to insert concept verification", &error))?;
    }
    Ok(())
}

/// Bulk-insert the `concept_provenance_source` rows, one array-unnest `INSERT`
/// per [`BATCH_SIZE`] chunk.
fn insert_source_rows(bundle_id: i64, rows: &ProvenanceRows) -> Result<(), CatalogError> {
    const INSERT: &str = "
        INSERT INTO pgokf.concept_provenance_source
            (bundle_id, tenant_id, concept_id, ordinal, source_id, resource, title, author,
             usage_count, last_modified, usage_window_from, usage_window_to)
        SELECT
            $1,
            (SELECT b.tenant_id FROM pgokf.bundles b WHERE b.id = $1),
            d.concept_id, d.ordinal, d.source_id, d.resource, d.title,
            d.author, d.usage_count,
            pg_catalog.to_timestamp(d.last_modified),
            pg_catalog.to_timestamp(d.usage_window_from),
            pg_catalog.to_timestamp(d.usage_window_to)
        FROM unnest(
                 $2::text[], $3::integer[], $4::text[], $5::text[], $6::text[],
                 $7::text[], $8::bigint[], $9::float8[], $10::float8[], $11::float8[])
             AS d(concept_id, ordinal, source_id, resource, title, author,
                   usage_count, last_modified, usage_window_from, usage_window_to)";

    let total = rows.source_concept_ids.len();
    for start in (0..total).step_by(BATCH_SIZE) {
        let end = usize::min(start + BATCH_SIZE, total);
        Spi::run_with_args(
            INSERT,
            &[
                bundle_id.into(),
                rows.source_concept_ids[start..end].to_vec().into(),
                rows.source_ordinals[start..end].to_vec().into(),
                rows.source_ids[start..end].to_vec().into(),
                rows.source_resources[start..end].to_vec().into(),
                rows.source_titles[start..end].to_vec().into(),
                rows.source_authors[start..end].to_vec().into(),
                rows.source_usage_counts[start..end].to_vec().into(),
                rows.source_last_modified[start..end].to_vec().into(),
                rows.source_usage_window_from[start..end].to_vec().into(),
                rows.source_usage_window_to[start..end].to_vec().into(),
            ],
        )
        .map_err(|error| spi_error("failed to insert concept provenance source", &error))?;
    }
    Ok(())
}

/// Project OKF v0.2 provenance/trust/lifecycle data for every staged concept.
///
/// Invoked inside the sync transaction after
/// [`crate::catalog::links::project`] and before the bundle row is finalized.
/// Every staged concept's existing rows are cleared from all three provenance
/// tables, and each concept that carries recognized provenance frontmatter is
/// re-inserted: one scalar `concept_provenance` row (typed columns, derived
/// trust tier, and the lossless `details` payload), plus one
/// `concept_verification` row per `verified[]` event and one
/// `concept_provenance_source` row per `sources[]` entry. Concepts without such
/// frontmatter produce no rows. Both phases are set-based and chunked at
/// [`BATCH_SIZE`].
///
/// # Errors
///
/// Returns a [`CatalogError`] on any SPI failure, aborting the surrounding sync
/// transaction so a partial projection is never committed.
pub fn project(bundle_id: i64, staged: &[StagedConcept]) -> Result<(), CatalogError> {
    delete_staged_provenance(bundle_id, staged)?;
    let rows = collect_provenance_rows(staged);
    insert_scalar_rows(bundle_id, &rows)?;
    insert_verification_rows(bundle_id, &rows)?;
    insert_source_rows(bundle_id, &rows)?;
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

    /// Wrap a parsed concept as a [`StagedConcept`] for the batch collectors.
    fn stage(concept: ParsedConcept) -> StagedConcept {
        StagedConcept {
            concept,
            file_hash: "hash".to_owned(),
            modified_at_epoch: Some(1.5),
            raw_content: None,
        }
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
    fn extract_scalar_maps_full_okf_v0_2_frontmatter() {
        // Arrange: the rich OKF v0.2 shape - nested generated, a usage_window,
        // a lifecycle status, and a stale_after instant.
        let concept = parse(
            "type: Attested Computation\n\
             title: Monthly active accounts\n\
             status: stable\n\
             stale_after: 2027-01-01T00:00:00Z\n\
             generated:\n  by: catalog-agent/1.0\n  at: 2026-07-01T12:00:00Z\n\
             usage_window:\n  from: 2026-06-01T00:00:00Z\n  to: 2026-06-30T23:59:59Z\n",
        );
        let events = extract_verification_events(&concept);

        // Act
        let scalar = extract_scalar(&concept, &events);

        // Assert
        assert_eq!(scalar.generated_by.as_deref(), Some("catalog-agent/1.0"));
        assert_eq!(
            scalar.generated_at,
            parse_iso8601_epoch("2026-07-01T12:00:00Z")
        );
        assert_eq!(scalar.status.as_deref(), Some("stable"));
        assert_eq!(
            scalar.stale_after,
            parse_iso8601_epoch("2027-01-01T00:00:00Z")
        );
        assert_eq!(
            scalar.usage_window_from,
            parse_iso8601_epoch("2026-06-01T00:00:00Z")
        );
        assert_eq!(
            scalar.usage_window_to,
            parse_iso8601_epoch("2026-06-30T23:59:59Z")
        );
    }

    #[test]
    fn extract_scalar_tolerates_bare_producer_spellings() {
        // Arrange: flat scalar spellings of generated.by / generated.at.
        let concept = parse(
            "type: Reference\n\
             title: Scalar provenance\n\
             generated_by: pipeline/9\n\
             generated_at: 2026-05-05T05:05:05Z\n",
        );
        let events = extract_verification_events(&concept);

        // Act
        let scalar = extract_scalar(&concept, &events);

        // Assert
        assert_eq!(scalar.generated_by.as_deref(), Some("pipeline/9"));
        assert_eq!(
            scalar.generated_at,
            parse_iso8601_epoch("2026-05-05T05:05:05Z")
        );
    }

    #[test]
    fn extract_scalar_coerces_wrong_typed_values_without_panicking() {
        // Arrange: generated as a number, a malformed stale_after - malformed
        // producer data that must degrade to None columns, never a panic.
        let concept = parse(
            "type: Reference\n\
             title: Malformed provenance\n\
             generated: 42\n\
             stale_after: soon\n\
             status: []\n",
        );
        let events = extract_verification_events(&concept);

        // Act
        let scalar = extract_scalar(&concept, &events);

        // Assert
        assert_eq!(scalar.generated_by, None);
        assert_eq!(scalar.generated_at, None);
        assert_eq!(scalar.stale_after, None);
        assert_eq!(scalar.status, None);
    }

    #[test]
    fn trust_tier_is_human_reviewed_when_a_human_verifies() {
        // Arrange: a process event followed by a human event.
        let concept = parse(
            "type: Reference\ntitle: Reviewed\n\
             verified:\n  - by: process:metric-validation\n    at: 2026-07-02T02:00:00Z\n\
             \x20\x20- by: human:fixture-reviewer\n    at: 2026-07-03T09:30:00Z\n",
        );

        // Act
        let events = extract_verification_events(&concept);

        // Assert: two events, ordered, with a human present → human-reviewed.
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].verified_by, "process:metric-validation");
        assert_eq!(events[1].verified_by, "human:fixture-reviewer");
        assert_eq!(trust_tier(&events), "human-reviewed");
    }

    #[test]
    fn trust_tier_is_machine_confirmed_without_a_human() {
        // Arrange: only a non-human verifier.
        let concept = parse(
            "type: Reference\ntitle: Machine checked\n\
             verified:\n  - by: process:metric-validation\n",
        );

        // Act
        let events = extract_verification_events(&concept);

        // Assert
        assert_eq!(trust_tier(&events), "machine-confirmed");
    }

    #[test]
    fn trust_tier_is_unverified_without_events() {
        // Arrange: provenance present but no verification.
        let concept = parse("type: Reference\ntitle: Unverified\nstatus: draft\n");

        // Act
        let events = extract_verification_events(&concept);

        // Assert
        assert!(events.is_empty());
        assert_eq!(trust_tier(&events), "unverified");
    }

    #[test]
    fn extract_verification_events_treats_a_single_mapping_as_one_event() {
        // Arrange: OKF v0.2 says a single verified mapping is a one-element list.
        let concept = parse(
            "type: Reference\ntitle: Single verify\n\
             verified:\n  by: human:solo\n  at: 2026-07-01T00:00:00Z\n",
        );

        // Act
        let events = extract_verification_events(&concept);

        // Assert
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].ordinal, 0);
        assert_eq!(events[0].verified_by, "human:solo");
    }

    #[test]
    fn extract_verification_events_skips_events_without_an_actor() {
        // Arrange: the second event has no `by`; verified_by is NOT NULL, so it
        // must be skipped, leaving its ordinal gap rather than renumbering.
        let concept = parse(
            "type: Reference\ntitle: Partial verify\n\
             verified:\n  - by: process:one\n  - at: 2026-07-01T00:00:00Z\n\
             \x20\x20- by: human:three\n",
        );

        // Act
        let events = extract_verification_events(&concept);

        // Assert: the actorless middle event is dropped; ordinals reflect source
        // position (0 and 2), never a fabricated NULL actor.
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].ordinal, 0);
        assert_eq!(events[1].ordinal, 2);
        assert_eq!(events[1].verified_by, "human:three");
    }

    #[test]
    fn extract_provenance_sources_maps_each_entry() {
        // Arrange: two sources, the second with a per-source usage_window.
        let concept = parse(
            "type: Reference\ntitle: Sourced\n\
             sources:\n\
             \x20\x20- id: account-policy\n    resource: https://docs.example.test/p\n\
             \x20\x20\x20\x20title: Active account policy\n    author: human:data-governance\n\
             \x20\x20\x20\x20usage_count: 4200\n    last_modified: 2026-06-15T08:00:00Z\n\
             \x20\x20- id: events-table\n    resource: /source-events.md\n\
             \x20\x20\x20\x20author: process:warehouse-catalog\n    usage_count: 18000\n\
             \x20\x20\x20\x20usage_window:\n      from: 2026-06-24T00:00:00Z\n      to: 2026-06-30T23:59:59Z\n",
        );

        // Act
        let sources = extract_provenance_sources(&concept);

        // Assert
        assert_eq!(sources.len(), 2);
        assert_eq!(sources[0].ordinal, 0);
        assert_eq!(sources[0].source_id.as_deref(), Some("account-policy"));
        assert_eq!(sources[0].author.as_deref(), Some("human:data-governance"));
        assert_eq!(sources[0].usage_count, Some(4200));
        assert_eq!(
            sources[0].last_modified,
            parse_iso8601_epoch("2026-06-15T08:00:00Z")
        );
        assert_eq!(sources[1].usage_count, Some(18_000));
        assert_eq!(
            sources[1].usage_window_from,
            parse_iso8601_epoch("2026-06-24T00:00:00Z")
        );
    }

    #[test]
    fn extract_provenance_sources_skips_non_object_entries() {
        // Arrange: a bare-string source entry is not a structured material.
        let concept = parse(
            "type: Reference\ntitle: Loose sources\n\
             sources:\n  - just-a-string\n  - id: real\n    resource: https://example.test\n",
        );

        // Act
        let sources = extract_provenance_sources(&concept);

        // Assert: only the object entry is materialized, keeping its source
        // position as the ordinal.
        assert_eq!(sources.len(), 1);
        assert_eq!(sources[0].ordinal, 1);
        assert_eq!(sources[0].source_id.as_deref(), Some("real"));
    }

    #[test]
    fn extract_details_retains_only_provenance_keys() {
        // Arrange: provenance keys mixed with type-specific and producer extras
        // that belong in concept_metadata, not the provenance details.
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

        // Assert: only the provenance/trust/lifecycle subset survives; type-
        // specific keys (parameters, runtime, computation) are not retained.
        assert_eq!(
            detail_keys(&details),
            vec![
                "generated".to_owned(),
                "sources".to_owned(),
                "stale_after".to_owned(),
                "status".to_owned(),
                "usage_window".to_owned(),
                "verified".to_owned(),
            ]
        );
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
    fn collect_provenance_rows_is_sparse_and_populates_child_rows() {
        // Arrange: one rich concept, one plain concept.
        let staged = vec![
            stage(parse(
                "type: Attested Computation\ntitle: Rich\n\
                 status: stable\n\
                 generated:\n  by: catalog-agent/1.0\n  at: 2026-07-01T12:00:00Z\n\
                 verified:\n  - by: process:metric-validation\n    at: 2026-07-02T02:00:00Z\n\
                 \x20\x20- by: human:fixture-reviewer\n    at: 2026-07-03T09:30:00Z\n\
                 sources:\n  - id: account-policy\n    resource: https://docs.example.test/p\n\
                 \x20\x20- id: events-table\n    resource: /source-events.md\n",
            )),
            stage(parse("type: Reference\ntitle: Plain\n")),
        ];

        // Act
        let rows = collect_provenance_rows(&staged);

        // Assert: only the rich concept produces a scalar row, with two
        // verification events and two provenance sources.
        assert_eq!(rows.scalar_concept_ids, vec!["concept".to_owned()]);
        assert_eq!(
            rows.generated_by,
            vec![Some("catalog-agent/1.0".to_owned())]
        );
        assert_eq!(rows.trust_tier, vec!["human-reviewed".to_owned()]);
        assert_eq!(rows.verification_concept_ids.len(), 2);
        assert_eq!(
            rows.verified_by,
            vec![
                "process:metric-validation".to_owned(),
                "human:fixture-reviewer".to_owned(),
            ]
        );
        assert_eq!(rows.source_concept_ids.len(), 2);
        assert_eq!(rows.source_ordinals, vec![0, 1]);
    }

    #[test]
    fn collect_provenance_rows_skips_concepts_without_provenance() {
        // Arrange: only core frontmatter, no provenance keys.
        let staged = vec![stage(parse("type: Reference\ntitle: Plain concept\n"))];

        // Act
        let rows = collect_provenance_rows(&staged);

        // Assert: no scalar row and no child rows.
        assert!(rows.scalar_concept_ids.is_empty());
        assert!(rows.verification_concept_ids.is_empty());
        assert!(rows.source_concept_ids.is_empty());
    }
}
