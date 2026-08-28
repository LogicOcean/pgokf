-- pgokf extension upgrade: 0.1.7 -> 0.1.8
--
-- This upgrade carries the COMPLETE delta from a fresh 0.1.7 install up to a
-- fresh 0.1.8 install, so that `ALTER EXTENSION pgokf UPDATE TO '0.1.8'` yields a
-- catalog functionally identical to `CREATE EXTENSION pgokf` at 0.1.8. The whole
-- file runs in a single transaction.
--
-- 0.1.8 is one additive lifecycle/audit feature batch:
--   F1 a per-concept change manifest for each sync
--      (pgokf_private.sync_log_change, pgokf.list_sync_changes);
--   F2 a soft-delete / retirement window on bundles
--      (bundles.retired_at, pgokf.retire_bundle / unretire_bundle / purge_retired);
--   F3 an exfiltration / access audit
--      (pgokf_private.access_log, pgokf.list_access_log) written by the three
--      content-exporting functions;
--   F4 cross-bundle content-duplicate detection (pgokf.duplicate_concepts).
--
-- Every statement is additive and non-destructive: no row is truncated or
-- deleted, and no data-bearing object is dropped. Behavior that lives entirely
-- in the 0.1.8 shared library needs no SQL here: the sync engine now writes the
-- change manifest; concept_search / concept_neighbors / semantic + hybrid search
-- now exclude retired bundles (active = enabled AND retired_at IS NULL); the two
-- exports and get_concept_source append access-log rows. Loading the new module
-- on update activates all of that. This script only creates the SQL objects
-- those code paths read and write, and re-points the two functions whose
-- security/comment changed.

-- ===========================================================================
-- F1, Step 1: the per-concept change manifest table
-- (pgokf_private.sync_log_change).
--
-- Administrator-only; the sync engine appends to it under owner rights and the
-- reader-granted pgokf.list_sync_changes reads it. It hangs off the parent
-- pgokf_private.sync_log row (ON DELETE CASCADE), so retention pruning of the
-- parent drops the manifest too. This matches, statement for statement, the
-- fresh 0.1.8 sync_log_change_table block.
-- ===========================================================================
CREATE TABLE pgokf_private.sync_log_change (
    sync_id     bigint NOT NULL REFERENCES pgokf_private.sync_log (id) ON DELETE CASCADE,
    tenant_id   text NOT NULL DEFAULT 'default',
    bundle_id   bigint,
    concept_id  text,
    change_kind text CHECK (change_kind IN ('added', 'updated', 'removed'))
);

CREATE INDEX sync_log_change_sync_id_idx ON pgokf_private.sync_log_change (sync_id);

ALTER TABLE pgokf_private.sync_log_change ENABLE ROW LEVEL SECURITY;
CREATE POLICY sync_log_change_tenant_isolation ON pgokf_private.sync_log_change
    USING (pg_catalog.current_setting('pgokf.tenant', true) IS NULL
        OR pg_catalog.current_setting('pgokf.tenant', true) = ''
        OR tenant_id = pg_catalog.current_setting('pgokf.tenant', true))
    WITH CHECK (pg_catalog.current_setting('pgokf.tenant', true) IS NULL
        OR pg_catalog.current_setting('pgokf.tenant', true) = ''
        OR tenant_id = pg_catalog.current_setting('pgokf.tenant', true));

REVOKE ALL ON pgokf_private.sync_log_change FROM PUBLIC;

COMMENT ON TABLE pgokf_private.sync_log_change IS
    'Per-concept change manifest for one sync: the concrete concepts a register/refresh/content sync added, updated, or removed, hung off the parent pgokf_private.sync_log row (ON DELETE CASCADE, so retention pruning of the parent drops the manifest too). Administrator-only; read through the reader-granted pgokf.list_sync_changes function.';
COMMENT ON COLUMN pgokf_private.sync_log_change.sync_id IS
    'Parent pgokf_private.sync_log.id this change belongs to; ON DELETE CASCADE ties the manifest to the audit row''s lifetime and retention window.';
COMMENT ON COLUMN pgokf_private.sync_log_change.tenant_id IS
    'Multi-tenant owner of the change, stamped from the parent bundle''s tenant_id; the row-level-security policy and the reader function apply the same opt-in pgokf.tenant filter.';
COMMENT ON COLUMN pgokf_private.sync_log_change.bundle_id IS
    'Identity of the bundle whose sync produced this change. FK-free (like sync_log.bundle_id) so the manifest survives the bundle''s later deletion.';
COMMENT ON COLUMN pgokf_private.sync_log_change.concept_id IS
    'The affected concept''s path-derived OKF id.';
COMMENT ON COLUMN pgokf_private.sync_log_change.change_kind IS
    'What happened to the concept in this sync: added, updated, or removed.';

-- F1, Step 2: the pgokf.sync_change composite and the reader-level
-- pgokf.list_sync_changes projection. SECURITY DEFINER over the admin-only,
-- tenant-scoped manifest table.
CREATE TYPE pgokf.sync_change AS (
    sync_id     bigint,
    bundle_id   bigint,
    concept_id  text,
    change_kind text
);

COMMENT ON TYPE pgokf.sync_change IS
    'One entry of a sync''s per-concept change manifest from pgokf.list_sync_changes: the parent sync id, the affected bundle and concept, and what happened to it (added/updated/removed).';

CREATE FUNCTION pgokf."list_sync_changes"(
    "sync_id" bigint,
    "max_rows" integer DEFAULT 1000
) RETURNS SETOF pgokf.sync_change
STRICT
LANGUAGE c
AS 'MODULE_PATHNAME', 'list_sync_changes_wrapper';

ALTER FUNCTION pgokf.list_sync_changes(bigint, integer)
    SECURITY DEFINER SET search_path = pg_catalog, pg_temp;
REVOKE ALL ON FUNCTION pgokf.list_sync_changes(bigint, integer) FROM PUBLIC;
GRANT EXECUTE ON FUNCTION pgokf.list_sync_changes(bigint, integer) TO pgokf_reader;
COMMENT ON FUNCTION pgokf.list_sync_changes(bigint, integer) IS
    'List the per-concept change manifest of one sync (pgokf_private.sync_log.id) as pgokf.sync_change: the concepts it added, updated, or removed, ordered by change_kind then concept_id and bounded by max_rows. Reader-level (SECURITY DEFINER over the admin-only manifest table, tenant-scoped); raises 22023 when max_rows < 0.';

-- ===========================================================================
-- F2: the bundle soft-delete / retirement window.
--
-- retired_at is appended after tenant_id (matching the fresh 0.1.8 column order),
-- with IF NOT EXISTS so re-running is idempotent. A bundle is 'active' only when
-- enabled AND retired_at IS NULL; the library-side search/traversal queries
-- enforce that. list_bundles (below) excludes retired bundles by default.
-- ===========================================================================
ALTER TABLE pgokf.bundles
    ADD COLUMN IF NOT EXISTS retired_at timestamptz DEFAULT NULL;

COMMENT ON COLUMN pgokf.bundles.retired_at IS
    'When the bundle was retired (soft-deleted) via pgokf.retire_bundle, or NULL when active. A bundle is ''active'' only when enabled AND retired_at IS NULL: a retired bundle is excluded from concept_search, concept_neighbors, and the default list_bundles listing without deleting any rows, so pgokf.unretire_bundle fully restores it. Retirement is an undo window for the hard unregister cascade; pgokf.purge_retired hard-deletes bundles retired longer than a chosen interval. Set once and preserved across re-retirement (the original instant governs the purge window).';

-- The writer-tier retire / un-retire toggles and the admin-tier purge. Each is
-- SECURITY DEFINER (write access to the base tables stays with the extension
-- owner); the wrapper symbols are exported by the 0.1.8 shared library.
CREATE FUNCTION pgokf."retire_bundle"(
    "bundle_id" bigint
) RETURNS pgokf.bundle_info
STRICT
LANGUAGE c
AS 'MODULE_PATHNAME', 'retire_bundle_wrapper';

CREATE FUNCTION pgokf."unretire_bundle"(
    "bundle_id" bigint
) RETURNS pgokf.bundle_info
STRICT
LANGUAGE c
AS 'MODULE_PATHNAME', 'unretire_bundle_wrapper';

CREATE FUNCTION pgokf."purge_retired"(
    "older_than" interval DEFAULT '7 days'
) RETURNS bigint
STRICT
LANGUAGE c
AS 'MODULE_PATHNAME', 'purge_retired_wrapper';

ALTER FUNCTION pgokf.retire_bundle(bigint)
    SECURITY DEFINER SET search_path = pg_catalog, pg_temp;
ALTER FUNCTION pgokf.unretire_bundle(bigint)
    SECURITY DEFINER SET search_path = pg_catalog, pg_temp;
ALTER FUNCTION pgokf.purge_retired(interval)
    SECURITY DEFINER SET search_path = pg_catalog, pg_temp;
REVOKE ALL ON FUNCTION pgokf.retire_bundle(bigint) FROM PUBLIC;
REVOKE ALL ON FUNCTION pgokf.unretire_bundle(bigint) FROM PUBLIC;
REVOKE ALL ON FUNCTION pgokf.purge_retired(interval) FROM PUBLIC;
GRANT EXECUTE ON FUNCTION pgokf.retire_bundle(bigint) TO pgokf_writer;
GRANT EXECUTE ON FUNCTION pgokf.unretire_bundle(bigint) TO pgokf_writer;
GRANT EXECUTE ON FUNCTION pgokf.purge_retired(interval) TO pgokf_admin;
COMMENT ON FUNCTION pgokf.retire_bundle(bigint) IS
    'Retire (soft-delete) a bundle: set retired_at = now(), returning the updated pgokf.bundle_info. Writer-tier (pgokf_writer; admin inherits it). Excludes the bundle from concept_search, concept_neighbors, and the default list_bundles without deleting rows (reversible via unretire_bundle); idempotent (keeps the original retired_at); does not change enabled. Raises 22023 if the bundle_id is unknown.';
COMMENT ON FUNCTION pgokf.unretire_bundle(bigint) IS
    'Un-retire a bundle: clear retired_at, returning the updated pgokf.bundle_info. Writer-tier (pgokf_writer; admin inherits it); fully reverses retire_bundle. Raises 22023 if the bundle_id is unknown.';
COMMENT ON FUNCTION pgokf.purge_retired(interval) IS
    'Hard-delete every bundle whose retired_at is older than now() - older_than (default 7 days); returns the count purged. Admin-only (pgokf_admin). Each purge cascades concept/metadata/feature rows and writes an unregister audit row; a bundle retired within the window stays recoverable via unretire_bundle. unregister_bundle remains a separate immediate hard delete.';

-- list_bundles now excludes retired bundles by default (library-side change);
-- only its COMMENT changes here to match the fresh 0.1.8 wording.
COMMENT ON FUNCTION pgokf.list_bundles() IS
    'List every active (non-retired) registered bundle as pgokf.bundle_info, ordered by id. Reader-level. Retired bundles are excluded (reachable by id via bundle_info, and visible with their retired_at in catalog_stats); disabled-but-not-retired bundles are still listed.';

-- catalog_stat gains a retired_at attribute so retired bundles — hidden from
-- list_bundles — remain visible in catalog_stats. The catalog_stats function
-- itself is unchanged (it resolves the composite by name), so only the type is
-- altered. This is a documented strict superset: existing rows gain a NULL
-- retired_at, matching a fresh 0.1.8 catalog_stat.
ALTER TYPE pgokf.catalog_stat ADD ATTRIBUTE retired_at timestamptz;

COMMENT ON TYPE pgokf.catalog_stat IS
    'Per-bundle operational statistics from pgokf.catalog_stats: identity and state, indexed-concept / link / resolved-link counts, sync recency (last_synced_at, sync_age), a 24-hour staleness flag, and retired_at (the soft-delete/retirement instant, NULL when active) so retired bundles — hidden from list_bundles — remain visible here.';

-- ===========================================================================
-- F3: the exfiltration / access audit (pgokf_private.access_log).
--
-- Administrator-only; the three content-exporting functions append to it under
-- owner rights and the admin-granted pgokf.list_access_log reads it. Matches the
-- fresh 0.1.8 access_log_table block statement for statement.
-- ===========================================================================
CREATE TABLE pgokf_private.access_log (
    id         bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    tenant_id  text NOT NULL DEFAULT 'default',
    actor      text NOT NULL DEFAULT session_user,
    at         timestamptz NOT NULL DEFAULT now(),
    op         text CHECK (op IN ('export_parquet', 'export_sources', 'get_concept_source')),
    bundle_id  bigint,
    concept_id text,
    detail     text
);

CREATE INDEX access_log_at_idx ON pgokf_private.access_log (at);
CREATE INDEX access_log_bundle_id_idx ON pgokf_private.access_log (bundle_id);

ALTER TABLE pgokf_private.access_log ENABLE ROW LEVEL SECURITY;
CREATE POLICY access_log_tenant_isolation ON pgokf_private.access_log
    USING (pg_catalog.current_setting('pgokf.tenant', true) IS NULL
        OR pg_catalog.current_setting('pgokf.tenant', true) = ''
        OR tenant_id = pg_catalog.current_setting('pgokf.tenant', true))
    WITH CHECK (pg_catalog.current_setting('pgokf.tenant', true) IS NULL
        OR pg_catalog.current_setting('pgokf.tenant', true) = ''
        OR tenant_id = pg_catalog.current_setting('pgokf.tenant', true));

REVOKE ALL ON pgokf_private.access_log FROM PUBLIC;

COMMENT ON TABLE pgokf_private.access_log IS
    'Append-only exfiltration/access audit: one row per content-exporting operation (export_parquet, export_sources, get_concept_source) — who read or exported what, and when. Written inside the operation''s own transaction under owner rights, then pruned to the sync_log_retention_days policy. Administrator-only; read through the admin-granted pgokf.list_access_log function.';
COMMENT ON COLUMN pgokf_private.access_log.id IS
    'Surrogate primary key (GENERATED ALWAYS AS IDENTITY); monotonic append order of the access trail.';
COMMENT ON COLUMN pgokf_private.access_log.tenant_id IS
    'Multi-tenant owner of the access, stamped from pgokf.tenant (effective_tenant(); ''default'' when unset). The row-level-security policy and the reader function apply the same opt-in tenant filter so a tenant session sees only its own access rows.';
COMMENT ON COLUMN pgokf_private.access_log.actor IS
    'The session_user that performed the operation, captured by column default.';
COMMENT ON COLUMN pgokf_private.access_log.at IS
    'When the operation committed (transaction now()); the pruning compares against sync_log_retention_days.';
COMMENT ON COLUMN pgokf_private.access_log.op IS
    'The exfiltration operation: export_parquet / export_sources / get_concept_source.';
COMMENT ON COLUMN pgokf_private.access_log.bundle_id IS
    'Identity of the bundle whose content was read or exported. FK-free so the row survives the bundle''s later deletion.';
COMMENT ON COLUMN pgokf_private.access_log.concept_id IS
    'The specific concept read, for get_concept_source; NULL for the whole-bundle exports.';
COMMENT ON COLUMN pgokf_private.access_log.detail IS
    'Optional free-text context (for the exports, the resolved destination directory).';

-- The pgokf.access_log_entry composite and the admin-tier pgokf.list_access_log
-- projection. SECURITY DEFINER over the admin-only, tenant-scoped access log.
CREATE TYPE pgokf.access_log_entry AS (
    id         bigint,
    actor      text,
    at         timestamptz,
    op         text,
    bundle_id  bigint,
    concept_id text,
    detail     text
);

COMMENT ON TYPE pgokf.access_log_entry IS
    'One exfiltration/access-audit entry from pgokf.list_access_log: the operation (export_parquet/export_sources/get_concept_source), who ran it, when, the affected bundle and concept, and optional detail.';

CREATE FUNCTION pgokf."list_access_log"(
    "bundle_id" bigint DEFAULT NULL,
    "max_rows" integer DEFAULT 100
) RETURNS SETOF pgokf.access_log_entry
LANGUAGE c
AS 'MODULE_PATHNAME', 'list_access_log_wrapper';

ALTER FUNCTION pgokf.list_access_log(bigint, integer)
    SECURITY DEFINER SET search_path = pg_catalog, pg_temp;
REVOKE ALL ON FUNCTION pgokf.list_access_log(bigint, integer) FROM PUBLIC;
GRANT EXECUTE ON FUNCTION pgokf.list_access_log(bigint, integer) TO pgokf_admin;
COMMENT ON FUNCTION pgokf.list_access_log(bigint, integer) IS
    'List recent pgokf_private.access_log exfiltration-audit entries as pgokf.access_log_entry, newest first, optionally scoped to one bundle and bounded by max_rows. Admin-only (SECURITY DEFINER over the admin-only, tenant-scoped access log); raises 22023 when max_rows < 0.';

-- get_concept_source becomes SECURITY DEFINER (and tenant-scopes its reads in
-- the library) so it can append a get_concept_source row to the access log; its
-- grant stays reader-level and its signature is unchanged. Re-point it and
-- refresh its comment to match the fresh 0.1.8 hardening.
ALTER FUNCTION pgokf.get_concept_source(bigint, text)
    SECURITY DEFINER SET search_path = pg_catalog, pg_temp;
COMMENT ON FUNCTION pgokf.get_concept_source(bigint, text) IS
    'Return the verbatim stored source bytes of one concept as bytea. Reader-level (same disclosure as body_text), SECURITY DEFINER and tenant-scoped so it can append one get_concept_source row to the exfiltration access log on each successful read. Raises 22023 when the concept exists but no source was stored, or when no such concept exists.';

-- ===========================================================================
-- F4: cross-bundle content-duplicate detection (pgokf.duplicate_concepts).
--
-- Reader-tier, invoker rights (reads only reader-granted pgokf.concepts,
-- RLS-filtered by tenant). Groups byte-identical concepts by their BLAKE3
-- file_hash. Matches the fresh 0.1.8 duplicate_group_type / duplicate_concepts.
-- ===========================================================================
CREATE TYPE pgokf.duplicate_group AS (
    file_hash   text,
    occurrences bigint,
    bundle_ids  bigint[],
    concept_ids text[]
);

COMMENT ON TYPE pgokf.duplicate_group IS
    'One group of byte-identical concepts from pgokf.duplicate_concepts: the shared BLAKE3 file_hash, how many concepts share it, and the parallel bundle_ids / concept_ids arrays of every occurrence (ordered by bundle then concept id).';

CREATE FUNCTION pgokf."duplicate_concepts"(
    "bundle_id" bigint DEFAULT NULL,
    "min_group" integer DEFAULT 2
) RETURNS SETOF pgokf.duplicate_group
STABLE PARALLEL SAFE
LANGUAGE c
AS 'MODULE_PATHNAME', 'duplicate_concepts_wrapper';

REVOKE ALL ON FUNCTION pgokf.duplicate_concepts(bigint, integer) FROM PUBLIC;
GRANT EXECUTE ON FUNCTION pgokf.duplicate_concepts(bigint, integer) TO pgokf_reader;
COMMENT ON FUNCTION pgokf.duplicate_concepts(bigint, integer) IS
    'Group byte-identical concepts by BLAKE3 file_hash (HAVING count(*) >= min_group, default 2) as pgokf.duplicate_group, so an operator can find the same content copied across bundles. Reader-level, STABLE, invoker rights (RLS-filtered to the tenant). Optional bundle_id keeps only groups touching that bundle (still listing every occurrence). Raises 22023 when min_group < 1.';
