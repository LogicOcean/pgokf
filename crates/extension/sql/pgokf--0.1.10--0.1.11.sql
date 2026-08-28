-- pgokf extension upgrade: 0.1.10 -> 0.1.11
--
-- This upgrade carries the COMPLETE delta from a fresh 0.1.10 install up to a
-- fresh 0.1.11 install, so that `ALTER EXTENSION pgokf UPDATE TO '0.1.11'` yields
-- a catalog functionally identical to `CREATE EXTENSION pgokf` at 0.1.11. The
-- whole file runs in a single transaction.
--
-- 0.1.11 is one additive, OPT-IN feature: concept version history + point-in-time
-- queries (SCD Type-2 temporal history).
--
--   D1 track_history / history_retention_days configuration keys — two new
--      columns on the singleton pgokf_private.config row. track_history (boolean,
--      default false) is the opt-in switch; when OFF (the default) nothing below
--      records a single row and an existing install behaves EXACTLY as before with
--      ZERO extra storage. history_retention_days (integer, default 0 = keep
--      indefinitely) bounds growth of closed versions.
--   D2 pgokf.concept_history — an append-only SCD-2 version trail of each concept,
--      one row per version with a validity interval [valid_from, valid_to). New
--      table; cascades from pgokf.bundles (NOT pgokf.concepts) so a removed
--      concept keeps its history until the bundle is unregistered.
--   D4 pgokf.concept_version composite + the reader functions pgokf.concept_history
--      and pgokf.concept_as_of.
--
-- The recording logic (D3) and the retention prune (D5) live entirely in the
-- 0.1.11 shared library, gated on track_history, and need no SQL here: loading the
-- new module on update activates them. This script only creates the SQL objects
-- those code paths read and write. Every statement is additive and
-- non-destructive: no row is truncated or deleted, and no data-bearing object is
-- dropped. The two config columns are backfilled by their DEFAULTs (history OFF,
-- retention 0), so an upgraded catalog starts with history disabled exactly as a
-- fresh 0.1.11 install does.

-- ===========================================================================
-- D1: the two new pgokf_private.config columns.
--
-- Appended (ALTER TABLE ... ADD COLUMN) after embedding_dim, matching the column
-- layout of the fresh 0.1.11 config table (whose CREATE TABLE likewise places
-- track_history / history_retention_days last). The NOT NULL DEFAULTs backfill the
-- singleton row with history disabled and unbounded retention — identical to a
-- fresh install. IF NOT EXISTS keeps each step idempotent. The non-negativity
-- CHECK on history_retention_days matches the fresh table constraint.
-- ===========================================================================
ALTER TABLE pgokf_private.config
    ADD COLUMN IF NOT EXISTS track_history boolean NOT NULL DEFAULT false;
ALTER TABLE pgokf_private.config
    ADD COLUMN IF NOT EXISTS history_retention_days integer NOT NULL DEFAULT 0;

DO $pgokf_history_retention_chk$
BEGIN
    ALTER TABLE pgokf_private.config
        ADD CONSTRAINT config_history_retention_nonneg_chk
        CHECK (history_retention_days >= 0);
EXCEPTION WHEN duplicate_object THEN
    NULL;
END
$pgokf_history_retention_chk$;

COMMENT ON COLUMN pgokf_private.config.track_history IS
    'Whether a register/refresh/content sync records an append-only SCD-2 version trail of each changed concept into pgokf.concept_history (true = keep point-in-time history; storage/retention tradeoff) or records nothing (false, the default). Off by default so an existing install, and any bundle synced with history disabled, behaves exactly as before with zero extra storage. Not retroactive: enabling it starts recording at the next sync; a concept first versioned after it was enabled begins at version 1 with the change_kind of that sync.';
COMMENT ON COLUMN pgokf_private.config.history_retention_days IS
    'Retention window in days for CLOSED pgokf.concept_history versions (valid_to IS NOT NULL): closed versions whose valid_to predates now() - this many days are pruned in the same transaction after each successful sync appends its history, when track_history is on. The single current open version of a concept (valid_to IS NULL) is never pruned. 0 (the default) keeps history indefinitely; must be >= 0.';

-- ===========================================================================
-- D2: the pgokf.concept_history projection table.
--
-- One row per concept version, keyed by (bundle_id, concept_id, version).
-- Reproduces, statement for statement, the fresh 0.1.11 concept_history_table
-- block: the CHECK on change_kind, the FK to pgokf.bundles (NOT pgokf.concepts,
-- so a removed concept keeps its history), the (bundle_id, concept_id, valid_from)
-- lookup index, opt-in-by-usage multi-tenant row-level security on the
-- denormalized tenant_id, the comments, and the reader SELECT grant.
-- ===========================================================================
CREATE TABLE pgokf.concept_history (
    bundle_id   bigint      NOT NULL,
    concept_id  text        NOT NULL,
    tenant_id   text        NOT NULL DEFAULT 'default',
    version     bigint      NOT NULL,
    valid_from  timestamptz NOT NULL,
    valid_to    timestamptz,
    change_kind text        NOT NULL,
    type        text,
    title       text,
    description text,
    tags        text[],
    resource    jsonb,
    body_text   text,
    file_hash   text,
    CONSTRAINT concept_history_pkey PRIMARY KEY (bundle_id, concept_id, version),
    CONSTRAINT concept_history_change_kind_chk
        CHECK (change_kind IN ('added', 'updated', 'removed')),
    CONSTRAINT concept_history_bundle_fk
        FOREIGN KEY (bundle_id)
        REFERENCES pgokf.bundles (id)
        ON DELETE CASCADE
);

CREATE INDEX concept_history_lookup_idx
    ON pgokf.concept_history (bundle_id, concept_id, valid_from);

ALTER TABLE pgokf.concept_history ENABLE ROW LEVEL SECURITY;
CREATE POLICY concept_history_tenant_isolation ON pgokf.concept_history
    USING (pg_catalog.current_setting('pgokf.tenant', true) IS NULL
        OR pg_catalog.current_setting('pgokf.tenant', true) = ''
        OR tenant_id = pg_catalog.current_setting('pgokf.tenant', true))
    WITH CHECK (pg_catalog.current_setting('pgokf.tenant', true) IS NULL
        OR pg_catalog.current_setting('pgokf.tenant', true) = ''
        OR tenant_id = pg_catalog.current_setting('pgokf.tenant', true));

COMMENT ON TABLE pgokf.concept_history IS
    'Opt-in append-only SCD Type-2 version trail of each concept, populated only when the track_history configuration key is enabled. Each row is one version with a validity interval [valid_from, valid_to) (valid_to IS NULL = the current open version); versions are per-concept monotonic and contiguous. Cascades from pgokf.bundles (NOT pgokf.concepts), so a removed concept keeps its history until the bundle is unregistered. Read through pgokf.concept_history(bundle_id, concept_id, max_rows) and pgokf.concept_as_of(bundle_id, concept_id, as_of).';
COMMENT ON COLUMN pgokf.concept_history.bundle_id IS
    'Bundle the versioned concept belongs to (references pgokf.bundles.id; ON DELETE CASCADE).';
COMMENT ON COLUMN pgokf.concept_history.concept_id IS
    'The versioned concept''s path-derived OKF id. Retained across the concept''s deletion (the FK is to the bundle, not the concept), so a removed concept''s history survives until the bundle is unregistered.';
COMMENT ON COLUMN pgokf.concept_history.tenant_id IS
    'Multi-tenant owner, denormalized from the concept''s bundle for a local row-level-security predicate; always equals the bundle''s tenant_id.';
COMMENT ON COLUMN pgokf.concept_history.version IS
    'Per-concept monotonic version number (1 for the first recorded version, prev+1 for each subsequent one). Part of the primary key with (bundle_id, concept_id).';
COMMENT ON COLUMN pgokf.concept_history.valid_from IS
    'Instant this version became valid (the sync transaction now()). Equals the prior version''s valid_to, so intervals are contiguous.';
COMMENT ON COLUMN pgokf.concept_history.valid_to IS
    'Instant this version stopped being valid, or NULL for the single current open version of a live concept. A removal tombstone is zero-width (valid_from = valid_to), so an as-of query at or after the removal instant returns no row.';
COMMENT ON COLUMN pgokf.concept_history.change_kind IS
    'What produced this version: added (version 1 of a new concept), updated (a content change), or removed (a zero-width tombstone marking deletion).';
COMMENT ON COLUMN pgokf.concept_history.type IS
    'Snapshot of the concept''s OKF type at this version; NULL for a removal tombstone.';
COMMENT ON COLUMN pgokf.concept_history.title IS
    'Snapshot of the concept''s title at this version; NULL for a removal tombstone.';
COMMENT ON COLUMN pgokf.concept_history.description IS
    'Snapshot of the concept''s description at this version; NULL when the concept had none, or for a removal tombstone.';
COMMENT ON COLUMN pgokf.concept_history.tags IS
    'Snapshot of the concept''s tags at this version; NULL for a removal tombstone.';
COMMENT ON COLUMN pgokf.concept_history.resource IS
    'Snapshot of the concept''s resource declaration (as jsonb) at this version; NULL when the concept had none, or for a removal tombstone.';
COMMENT ON COLUMN pgokf.concept_history.body_text IS
    'Snapshot of the concept''s search-indexed plain-text body at this version; NULL for a removal tombstone.';
COMMENT ON COLUMN pgokf.concept_history.file_hash IS
    'Snapshot of the concept''s source-file BLAKE3 digest at this version; NULL for a removal tombstone.';

GRANT SELECT ON pgokf.concept_history TO pgokf_reader;

-- ===========================================================================
-- D4, Step 1: the pgokf.concept_version composite type.
--
-- The row shape returned by both reader functions. Reproduces the fresh 0.1.11
-- concept_version_type block.
-- ===========================================================================
CREATE TYPE pgokf.concept_version AS (
    version     bigint,
    valid_from  timestamptz,
    valid_to    timestamptz,
    change_kind text,
    type        text,
    title       text,
    description text,
    file_hash   text
);

COMMENT ON TYPE pgokf.concept_version IS
    'One version of a concept from pgokf.concept_history / pgokf.concept_as_of: the per-concept version number, its validity interval [valid_from, valid_to) (valid_to NULL = current), what produced it (change_kind), and a snapshot of the concept core (type, title, description, file_hash) at that version (NULL for a removal tombstone).';

-- ===========================================================================
-- D4, Step 2: the reader functions pgokf.concept_history and
-- pgokf.concept_as_of.
--
-- INVOKER rights (no SECURITY DEFINER) over the public projection table, so the
-- caller's own opt-in tenant row-level security applies — matching
-- concept_neighbors and list_bundle_log. Reproduces the fresh 0.1.11
-- pgrx-generated CREATE FUNCTIONs and the concept_history_function_hardening
-- block. (The concept_history TABLE and the concept_history FUNCTION share a name
-- across the pg_class / pg_proc catalogs, exactly as a fresh install has them.)
-- ===========================================================================
CREATE FUNCTION pgokf."concept_history"(
    "bundle_id" bigint, /* i64 */
    "concept_id" TEXT, /* & str */
    "max_rows" INT DEFAULT 100 /* i32 */
) RETURNS SETOF pgokf.concept_version /* :: pgrx :: heap_tuple :: PgHeapTuple < '_, :: pgrx :: pgbox :: AllocatedByRust > */
STRICT STABLE PARALLEL SAFE
LANGUAGE c /* Rust */
AS 'MODULE_PATHNAME', 'concept_history_wrapper';

CREATE FUNCTION pgokf."concept_as_of"(
    "bundle_id" bigint, /* i64 */
    "concept_id" TEXT, /* & str */
    "as_of" timestamp with time zone /* TimestampWithTimeZone */
) RETURNS SETOF pgokf.concept_version /* :: pgrx :: heap_tuple :: PgHeapTuple < '_, :: pgrx :: pgbox :: AllocatedByRust > */
STRICT STABLE PARALLEL SAFE
LANGUAGE c /* Rust */
AS 'MODULE_PATHNAME', 'concept_as_of_wrapper';

REVOKE ALL ON FUNCTION pgokf.concept_history(bigint, text, integer) FROM PUBLIC;
REVOKE ALL ON FUNCTION pgokf.concept_as_of(bigint, text, timestamptz) FROM PUBLIC;
GRANT EXECUTE ON FUNCTION pgokf.concept_history(bigint, text, integer) TO pgokf_reader;
GRANT EXECUTE ON FUNCTION pgokf.concept_as_of(bigint, text, timestamptz) TO pgokf_reader;
COMMENT ON FUNCTION pgokf.concept_history(bigint, text, integer) IS
    'List one concept''s recorded version timeline as pgokf.concept_version, newest version first, bounded by max_rows. Reader-level, STABLE, invoker rights (the caller''s tenant row-level security applies over pgokf.concept_history). Empty when track_history was off for the bundle''s syncs. Raises 22023 when max_rows < 0.';
COMMENT ON FUNCTION pgokf.concept_as_of(bigint, text, timestamptz) IS
    'Return the single concept version valid at as_of (valid_from <= as_of AND (valid_to IS NULL OR as_of < valid_to)) as pgokf.concept_version, or zero rows if the concept did not exist or had been removed at that instant. The point-in-time answer. Reader-level, STABLE, invoker rights (the caller''s tenant row-level security applies).';
