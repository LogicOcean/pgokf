-- pgokf extension upgrade: 0.1.2 -> 0.1.3
--
-- Re-models the OKF provenance/trust/lifecycle projection to faithfully match
-- the OKF v0.2 specification. Unlike the earlier additive upgrades, this one is
-- a DELIBERATE breaking change to pgokf.concept_provenance, permitted because
-- pgokf is pre-release (no tags, no external installs). It:
--
--   1. drops the invented columns (verified boolean, verification_method,
--      freshness) that had no OKF basis, and adds the spec-faithful scalar
--      columns (generated_at, status, stale_after, usage_window_from/to, and the
--      derived trust_tier) alongside the retained generated_by and details;
--   2. adds pgokf.concept_verification (the OKF verified[] event list);
--   3. adds pgokf.concept_provenance_source (the OKF sources[] materials -
--      distinct from pgokf.concept_source, which holds raw concept bytes);
--   4. begins populating pgokf.bundles.okf_version from a bundle-root index.md.
--
-- NON-RETROACTIVE: like store_source in 0.1.2, this migration does not back-fill
-- the new columns or child tables for already-synced bundles. Existing
-- concept_provenance rows keep NULL in every new column, and the new child
-- tables start empty, until pgokf.refresh_bundle re-indexes each bundle (which
-- re-projects it under the new model). bundles.okf_version likewise stays NULL
-- until the next sync/refresh reads the bundle-root index.md.
--
-- `ALTER EXTENSION pgokf UPDATE TO '0.1.3'` runs this in a single transaction.

-- 1. Re-model the scalar provenance table. Dropping the `verified` column also
--    drops its partial index automatically; the explicit DROP INDEX keeps the
--    step idempotent if the column was already removed.
DROP INDEX IF EXISTS pgokf.concept_provenance_verified_idx;

ALTER TABLE pgokf.concept_provenance
    DROP COLUMN IF EXISTS verified,
    DROP COLUMN IF EXISTS verification_method,
    DROP COLUMN IF EXISTS freshness,
    ADD COLUMN IF NOT EXISTS generated_at      timestamptz,
    ADD COLUMN IF NOT EXISTS status            text,
    ADD COLUMN IF NOT EXISTS stale_after       timestamptz,
    ADD COLUMN IF NOT EXISTS usage_window_from timestamptz,
    ADD COLUMN IF NOT EXISTS usage_window_to   timestamptz,
    ADD COLUMN IF NOT EXISTS trust_tier        text;

CREATE INDEX IF NOT EXISTS concept_provenance_trust_tier_idx
    ON pgokf.concept_provenance (trust_tier);

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

-- 2. The OKF verified[] event list, cascading from pgokf.concepts.
CREATE TABLE IF NOT EXISTS pgokf.concept_verification (
    bundle_id   bigint  NOT NULL,
    concept_id  text    NOT NULL,
    ordinal     integer NOT NULL,
    verified_by text    NOT NULL,
    verified_at timestamptz,
    CONSTRAINT concept_verification_pkey PRIMARY KEY (bundle_id, concept_id, ordinal),
    CONSTRAINT concept_verification_concept_fk
        FOREIGN KEY (bundle_id, concept_id)
        REFERENCES pgokf.concepts (bundle_id, id)
        ON DELETE CASCADE
);

COMMENT ON TABLE pgokf.concept_verification IS
    'One row per OKF v0.2 verified[] event for a concept: the ordered list of verification events (a single mapping is stored as one 0-ordinal row). Cascades from pgokf.concepts.';
COMMENT ON COLUMN pgokf.concept_verification.ordinal IS
    'Zero-based position of the event in the concept''s verified[] list; forms the primary key with (bundle_id, concept_id).';
COMMENT ON COLUMN pgokf.concept_verification.verified_by IS
    'OKF verified[].by: the actor that performed the verification (agent/human:/process:). Events with no actor are skipped, never stored as NULL.';
COMMENT ON COLUMN pgokf.concept_verification.verified_at IS
    'OKF verified[].at, parsed from ISO 8601. NULL when the at value is absent or unparseable.';

GRANT SELECT ON pgokf.concept_verification TO pgokf_reader;

-- 3. The OKF sources[] provenance materials, cascading from pgokf.concepts.
CREATE TABLE IF NOT EXISTS pgokf.concept_provenance_source (
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
    CONSTRAINT concept_provenance_source_pkey PRIMARY KEY (bundle_id, concept_id, ordinal),
    CONSTRAINT concept_provenance_source_concept_fk
        FOREIGN KEY (bundle_id, concept_id)
        REFERENCES pgokf.concepts (bundle_id, id)
        ON DELETE CASCADE
);

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

GRANT SELECT ON pgokf.concept_provenance_source TO pgokf_reader;
