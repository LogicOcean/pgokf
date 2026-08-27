//! Base-table DDL for the catalog projection.
//!
//! Everything here ships as the single named SQL block **`catalog_tables`**,
//! ordered directly after the `bootstrap` block. Feature modules (links,
//! provenance, config, admin) must order their own SQL after the base schema
//! with `requires = ["catalog_tables"]` and must never redefine objects owned
//! by this block.
//!
//! Objects owned by `catalog_tables`:
//!
//! - tables `pgokf.bundles`, `pgokf.concepts`, `pgokf.concept_metadata`
//! - their indexes (GIN on `tags`, `body_tsv`, `concept_metadata.value`;
//!   btree on `concepts.type` and `concepts.path`)
//! - composite result types `pgokf.bundle_sync_result` and
//!   `pgokf.concept_search_result`
//! - `SELECT` grants for `pgokf_reader` (write access stays with the
//!   extension owner; mutations go through the `SECURITY DEFINER` sync
//!   functions)
//!
//! The `pgokf.bundle_info` composite type is deliberately **not** created
//! here: neither register/refresh nor search needs it, so it belongs to the
//! admin feature wave (see [`crate::catalog::admin`]).

use pgrx::extension_sql;

extension_sql!(
    r"
CREATE TABLE pgokf.bundles (
    id             bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    path           text NOT NULL,
    name           text,
    okf_version    text,
    file_count     integer NOT NULL DEFAULT 0,
    last_synced_at timestamptz,
    sync_hash      text,
    options        jsonb NOT NULL DEFAULT '{}'::jsonb,
    enabled        boolean NOT NULL DEFAULT true,
    CONSTRAINT bundles_path_key UNIQUE (path)
);

CREATE TABLE pgokf.concepts (
    bundle_id   bigint NOT NULL REFERENCES pgokf.bundles (id) ON DELETE CASCADE,
    id          text NOT NULL,
    path        text NOT NULL,
    type        text,
    title       text,
    description text,
    tags        text[],
    resource    text,
    body_text   text NOT NULL DEFAULT '',
    file_hash   text NOT NULL,
    modified_at timestamptz,
    body_tsv    tsvector,
    indexed_at  timestamptz NOT NULL DEFAULT now(),
    CONSTRAINT concepts_pkey PRIMARY KEY (bundle_id, id),
    CONSTRAINT concepts_bundle_path_key UNIQUE (bundle_id, path)
);

CREATE TABLE pgokf.concept_metadata (
    bundle_id  bigint NOT NULL,
    concept_id text NOT NULL,
    key        text NOT NULL,
    value      jsonb NOT NULL,
    CONSTRAINT concept_metadata_key_uq UNIQUE (bundle_id, concept_id, key),
    CONSTRAINT concept_metadata_concept_fk
        FOREIGN KEY (bundle_id, concept_id)
        REFERENCES pgokf.concepts (bundle_id, id)
        ON DELETE CASCADE
);

CREATE INDEX concepts_tags_gin ON pgokf.concepts USING gin (tags);
CREATE INDEX concepts_body_tsv_gin ON pgokf.concepts USING gin (body_tsv);
CREATE INDEX concept_metadata_value_gin
    ON pgokf.concept_metadata USING gin (value jsonb_path_ops);
CREATE INDEX concepts_type_idx ON pgokf.concepts (type);
CREATE INDEX concepts_path_idx ON pgokf.concepts (path);

CREATE TYPE pgokf.bundle_sync_result AS (
    bundle_id bigint,
    path      text,
    added     integer,
    updated   integer,
    removed   integer,
    unchanged integer,
    total     integer
);

CREATE TYPE pgokf.concept_search_result AS (
    bundle_id  bigint,
    concept_id text,
    path       text,
    title      text,
    type       text,
    rank       real,
    headline   text
);

COMMENT ON TABLE pgokf.bundles IS
    'One registered OKF bundle root: canonical path, sync state, and aggregate digest.';
COMMENT ON TABLE pgokf.concepts IS
    'One row per (bundle_id, concept_id): the catalog projection of an OKF concept document.';
COMMENT ON TABLE pgokf.concept_metadata IS
    'Producer-defined frontmatter keys retained per concept as jsonb, one row per key.';
COMMENT ON TYPE pgokf.bundle_sync_result IS
    'Result of pgokf.register_bundle / pgokf.refresh_bundle: per-bucket file counts for one sync.';
COMMENT ON TYPE pgokf.concept_search_result IS
    'One ranked hit from pgokf.concept_search, with a ts_headline snippet.';
COMMENT ON COLUMN pgokf.concepts.id IS
    'Path-derived OKF concept ID: the normalized bundle-relative path without its .md suffix.';
COMMENT ON COLUMN pgokf.concepts.file_hash IS
    'Lowercase hexadecimal BLAKE3 digest of the source file; the identity used for incremental sync.';
COMMENT ON COLUMN pgokf.concepts.body_tsv IS
    'Weighted search vector: title (A), tags/type/description (B), body text (D).';
COMMENT ON COLUMN pgokf.bundles.sync_hash IS
    'Aggregate BLAKE3 digest over the sorted (path, file_hash) pairs of the last successful sync.';

GRANT SELECT ON pgokf.bundles, pgokf.concepts, pgokf.concept_metadata TO pgokf_reader;
",
    name = "catalog_tables",
    requires = ["bootstrap"]
);
