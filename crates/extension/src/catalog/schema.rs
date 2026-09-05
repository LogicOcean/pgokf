// SPDX-License-Identifier: AGPL-3.0-only
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
    source_type    text NOT NULL DEFAULT 'filesystem',
    -- tenant_id is appended last so a fresh install matches, column-for-column, an
    -- existing install upgraded via ADD COLUMN (see sql/pgokf--0.1.6--0.1.7.sql).
    tenant_id      text NOT NULL DEFAULT 'default',
    -- retired_at is appended after tenant_id for the same reason: a fresh install
    -- matches an existing install upgraded via ADD COLUMN (sql/pgokf--0.1.7--0.1.8.sql).
    retired_at     timestamptz DEFAULT NULL,
    CONSTRAINT bundles_tenant_path_key UNIQUE (tenant_id, path),
    CONSTRAINT bundles_source_type_chk CHECK (source_type IN ('filesystem', 'content'))
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
    tenant_id   text NOT NULL DEFAULT 'default',
    CONSTRAINT concepts_pkey PRIMARY KEY (bundle_id, id),
    CONSTRAINT concepts_bundle_path_key UNIQUE (bundle_id, path)
);

CREATE TABLE pgokf.concept_metadata (
    bundle_id  bigint NOT NULL,
    concept_id text NOT NULL,
    key        text NOT NULL,
    value      jsonb NOT NULL,
    tenant_id  text NOT NULL DEFAULT 'default',
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
-- Index the RLS discriminator on concepts (the highest-cardinality projection
-- table). On pgokf.bundles the UNIQUE (tenant_id, path) index already leads with
-- tenant_id, so the tenant predicate there is index-served without a second one.
CREATE INDEX concepts_tenant_id_idx ON pgokf.concepts (tenant_id);

-- Multi-tenant isolation. Every projection table carries a denormalized
-- tenant_id and enables row-level security with the identical opt-in-by-usage
-- predicate: a session that has NOT set pgokf.tenant (NULL or empty - every
-- pre-multi-tenancy install) sees ALL rows unchanged - unless the durable
-- policy key require_tenant is on, in which case it sees NONE (the
-- pgokf.tenant_required() sub-select is uncorrelated, so it is one InitPlan
-- per statement, never a per-row call) - while a session that HAS set it sees
-- only that tenant's rows. RLS is NOT forced, so the SECURITY DEFINER
-- write/admin functions (which run as the table owner) bypass it and may stamp
-- and read across tenants - correct because each operates strictly within one
-- single-tenant bundle. The matching WITH CHECK constrains any future
-- invoker-side write to the active tenant.
ALTER TABLE pgokf.bundles ENABLE ROW LEVEL SECURITY;
CREATE POLICY bundles_tenant_isolation ON pgokf.bundles
    USING (((pg_catalog.current_setting('pgokf.tenant', true) IS NULL
             OR pg_catalog.current_setting('pgokf.tenant', true) = '')
            AND NOT (SELECT pgokf.tenant_required()))
        OR tenant_id = pg_catalog.current_setting('pgokf.tenant', true))
    WITH CHECK (((pg_catalog.current_setting('pgokf.tenant', true) IS NULL
                  OR pg_catalog.current_setting('pgokf.tenant', true) = '')
                 AND NOT (SELECT pgokf.tenant_required()))
        OR tenant_id = pg_catalog.current_setting('pgokf.tenant', true));

ALTER TABLE pgokf.concepts ENABLE ROW LEVEL SECURITY;
CREATE POLICY concepts_tenant_isolation ON pgokf.concepts
    USING (((pg_catalog.current_setting('pgokf.tenant', true) IS NULL
             OR pg_catalog.current_setting('pgokf.tenant', true) = '')
            AND NOT (SELECT pgokf.tenant_required()))
        OR tenant_id = pg_catalog.current_setting('pgokf.tenant', true))
    WITH CHECK (((pg_catalog.current_setting('pgokf.tenant', true) IS NULL
                  OR pg_catalog.current_setting('pgokf.tenant', true) = '')
                 AND NOT (SELECT pgokf.tenant_required()))
        OR tenant_id = pg_catalog.current_setting('pgokf.tenant', true));

ALTER TABLE pgokf.concept_metadata ENABLE ROW LEVEL SECURITY;
CREATE POLICY concept_metadata_tenant_isolation ON pgokf.concept_metadata
    USING (((pg_catalog.current_setting('pgokf.tenant', true) IS NULL
             OR pg_catalog.current_setting('pgokf.tenant', true) = '')
            AND NOT (SELECT pgokf.tenant_required()))
        OR tenant_id = pg_catalog.current_setting('pgokf.tenant', true))
    WITH CHECK (((pg_catalog.current_setting('pgokf.tenant', true) IS NULL
                  OR pg_catalog.current_setting('pgokf.tenant', true) = '')
                 AND NOT (SELECT pgokf.tenant_required()))
        OR tenant_id = pg_catalog.current_setting('pgokf.tenant', true));

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
COMMENT ON COLUMN pgokf.bundles.retired_at IS
    'When the bundle was retired (soft-deleted) via pgokf.retire_bundle, or NULL when active. A bundle is ''active'' only when enabled AND retired_at IS NULL: a retired bundle is excluded from concept_search, concept_neighbors, and the default list_bundles listing without deleting any rows, so pgokf.unretire_bundle fully restores it. Retirement is an undo window for the hard unregister cascade; pgokf.purge_retired hard-deletes bundles retired longer than a chosen interval. Set once and preserved across re-retirement (the original instant governs the purge window).';
COMMENT ON COLUMN pgokf.bundles.source_type IS
    'How the bundle bytes reach the catalog: ''filesystem'' (registered from a canonical on-disk root via pgokf.register_bundle and refreshed from disk via pgokf.refresh_bundle) or ''content'' (streamed in memory via pgokf.register_bundle_content - a mountless object-store companion or any client - where path is the synthetic key ''content:''||name and refresh_bundle is rejected).';
COMMENT ON COLUMN pgokf.bundles.tenant_id IS
    'Multi-tenant owner of this bundle, stamped at registration from pgokf.tenant (effective_tenant(); ''default'' for a session that set no tenant). A bundle is single-tenant and its tenant never changes on refresh/unregister/enable; combined with path it forms the per-tenant registration key UNIQUE (tenant_id, path), so two tenants may register the same filesystem or content:<name> path. The row-level-security policy shows it only to a matching or unset pgokf.tenant.';
COMMENT ON COLUMN pgokf.concepts.tenant_id IS
    'Multi-tenant owner, denormalized from the concept''s bundle so the row-level-security predicate is local and index-friendly; always equals pgokf.bundles.tenant_id for the concept''s bundle.';
COMMENT ON COLUMN pgokf.concept_metadata.tenant_id IS
    'Multi-tenant owner, denormalized from the concept''s bundle for a local row-level-security predicate; always equals the bundle''s tenant_id.';

GRANT SELECT ON pgokf.bundles, pgokf.concepts, pgokf.concept_metadata TO pgokf_reader;
",
    name = "catalog_tables",
    requires = ["bootstrap"]
);

// The multi-tenant write helper. `effective_tenant()` resolves the tenant a
// write is stamped with from the per-session `pgokf.tenant` GUC - an unset or
// empty value maps to the literal 'default', matching the column default and the
// pre-multi-tenancy behavior. It lives in the administrator-only `pgokf_private`
// schema so it never widens the public `pgokf` API surface, and is called only
// from the SECURITY DEFINER write functions (which run as, and are owned by, the
// extension owner, so no additional grant is required). The row-level-security
// policies deliberately inline `current_setting('pgokf.tenant', true)` rather
// than call this helper, so a reader needs no access to `pgokf_private`.
extension_sql!(
    r"
CREATE FUNCTION pgokf_private.effective_tenant() RETURNS text
    LANGUAGE sql
    STABLE
    AS $$
        SELECT coalesce(
            nullif(pg_catalog.current_setting('pgokf.tenant', true), ''),
            'default')
    $$;

REVOKE ALL ON FUNCTION pgokf_private.effective_tenant() FROM PUBLIC;

COMMENT ON FUNCTION pgokf_private.effective_tenant() IS
    'Resolve the tenant that a catalog write is stamped with from the per-session pgokf.tenant GUC: an unset or empty value yields the literal ''default'' (matching the tenant_id column default and the pre-multi-tenancy behavior), any other value is the active tenant. Internal helper for the SECURITY DEFINER write paths; the row-level-security policies inline current_setting instead so readers need no access to pgokf_private.';
",
    name = "tenant_context",
    requires = ["catalog_tables"]
);

// The `finalize` block: pgrx orders it after every other SQL entity, so it
// hosts the two things that must see the completed catalog.
//
// 1. The `pgokf.version()` function is declared in `crate::lib`'s `pgokf`
//    schema module, outside any `requires`-addressable catalog block, so its
//    documentation comment ships here, once the generated
//    `CREATE FUNCTION pgokf.version()` exists. This closes the last gap in
//    COMMENT coverage of the public API surface without touching the
//    function's definition.
// 2. `pg_dump` skips the contents of extension-owned tables unless the
//    extension registers them as configuration tables. The bootstrap defines
//    `pgokf_private.register_dump_relations()`, which registers every
//    `pgokf.*` and `pgokf_private.*` table and sequence discovered from the
//    extension's own dependency graph; it is called here, once every relation
//    exists, so a logical backup carries the catalog's rows and sequence
//    positions (not just the CREATE EXTENSION statement). Every upgrade
//    script that adds a relation calls the same function as its last
//    statement. `pg_extension_config_dump` may only be called from an
//    extension script, which is why this lives in SQL.
extension_sql!(
    r"
COMMENT ON FUNCTION pgokf.version() IS
    'Return the version string of the loaded pgokf shared library (its Cargo package version). Immutable and parallel-safe; used to confirm the SQL extension and module agree after an upgrade.';

SELECT pgokf_private.register_dump_relations();
",
    name = "version_comment",
    finalize
);
