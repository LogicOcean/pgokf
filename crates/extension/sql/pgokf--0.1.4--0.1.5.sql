-- pgokf extension upgrade: 0.1.4 -> 0.1.5
--
-- This upgrade carries the COMPLETE delta from a fresh 0.1.4 install up to a
-- fresh 0.1.5 install, so that `ALTER EXTENSION pgokf UPDATE TO '0.1.5'` yields
-- a catalog functionally identical to `CREATE EXTENSION pgokf` at 0.1.5. It is
-- one additive feature batch — an audit/sync log, a bundle enable/disable
-- lifecycle, opt-in change notification, an observability surface, and OKF
-- version-conformance policy — so every statement is additive and
-- non-destructive: nothing is dropped or truncated, and existing rows keep
-- their values (the two new config columns backfill the singleton row from
-- their defaults). The whole file runs in a single transaction.
--
-- Behavior that lives entirely in the 0.1.5 shared library needs no SQL here:
-- the sync engine now appends a pgokf_private.sync_log row and prunes it to the
-- retention policy, emits the opt-in pg_notify, and enforces okf_version_policy;
-- concept_neighbors now excludes disabled bundles. Loading the new module on
-- update activates all of that. This script only creates the new SQL objects
-- those code paths read and write, and the two new configuration columns.

-- ===========================================================================
-- Step 1: new durable configuration columns (notify_channel, okf_version_policy)
--
-- Fresh 0.1.5 ships these two columns on pgokf_private.config; the sync engine
-- reads them (sync_defaults) and get_config projects them, so they MUST exist
-- before the 0.1.5 module runs a sync. Added with their NOT NULL defaults so the
-- existing singleton row backfills non-destructively; the policy CHECK is added
-- guarded so re-running is idempotent.
-- ===========================================================================
ALTER TABLE pgokf_private.config
    ADD COLUMN IF NOT EXISTS notify_channel text NOT NULL DEFAULT '';
ALTER TABLE pgokf_private.config
    ADD COLUMN IF NOT EXISTS okf_version_policy text NOT NULL DEFAULT 'warn';

DO $pgokf_okf_version_policy_chk$
BEGIN
    ALTER TABLE pgokf_private.config
        ADD CONSTRAINT config_okf_version_policy_chk
        CHECK (okf_version_policy IN ('warn', 'reject'));
EXCEPTION WHEN duplicate_object THEN
    NULL;
END
$pgokf_okf_version_policy_chk$;

COMMENT ON COLUMN pgokf_private.config.notify_channel IS
    'LISTEN/NOTIFY channel that a successful sync (register/refresh/register_bundle_content) announces on with a JSON payload {bundle_id, op, added, updated, removed, total}. Empty (the default) disables notification, with zero overhead. A non-empty value must be a safe channel identifier (letters, digits, underscore; leading letter or underscore; <= 63 bytes).';
COMMENT ON COLUMN pgokf_private.config.okf_version_policy IS
    'How sync treats a bundle-root index.md that declares an okf_version this build does not support (only 0.2 / 0.2.x is supported): ''warn'' (the default) logs a WARNING and indexes anyway, ''reject'' aborts the sync with 22023. An absent okf_version is always accepted and leaves pgokf.bundles.okf_version NULL.';

-- Refresh the sync_log_retention_days comment to the fresh-0.1.5 wording: the
-- knob is now live (it was defined but dead before this release).
COMMENT ON COLUMN pgokf_private.config.sync_log_retention_days IS
    'Retention window in days for pgokf_private.sync_log history: rows older than now() - this many days are pruned in the same transaction after each successful sync appends its audit row. 0 (or any value with no older rows) keeps history indefinitely; must be >= 0.';

-- ===========================================================================
-- Step 2: the sync/audit log table (pgokf_private.sync_log).
--
-- Administrator-only; the sync engine appends to it and prunes it, and the
-- reader-granted pgokf.list_sync_log function reads it. bundle_id is
-- intentionally FK-free so an unregister row survives the bundle's deletion.
-- ===========================================================================
CREATE TABLE pgokf_private.sync_log (
    id          bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    bundle_id   bigint,
    bundle_path text,
    op          text NOT NULL,
    actor       text NOT NULL DEFAULT session_user,
    synced_at   timestamptz NOT NULL DEFAULT now(),
    added       integer,
    updated     integer,
    removed     integer,
    unchanged   integer,
    total       integer,
    sync_hash   text,
    CONSTRAINT sync_log_op_chk CHECK (op IN ('register', 'refresh', 'content', 'unregister'))
);

CREATE INDEX sync_log_bundle_id_idx ON pgokf_private.sync_log (bundle_id);
CREATE INDEX sync_log_synced_at_idx ON pgokf_private.sync_log (synced_at);

REVOKE ALL ON pgokf_private.sync_log FROM PUBLIC;

COMMENT ON TABLE pgokf_private.sync_log IS
    'Append-only audit trail of catalog-mutating operations: one row per successful register/refresh/content sync or bundle unregister, written inside the operation''s own transaction under the bundle advisory lock (so a logged row always means the operation committed). History is pruned to the sync_log_retention_days policy after each append. Administrator-only; read through the reader-granted pgokf.list_sync_log function.';
COMMENT ON COLUMN pgokf_private.sync_log.id IS
    'Surrogate primary key (GENERATED ALWAYS AS IDENTITY); monotonic append order of the audit trail.';
COMMENT ON COLUMN pgokf_private.sync_log.bundle_id IS
    'Identity of the affected bundle. Retained for unregister rows even though the pgokf.bundles row is gone, so there is intentionally no foreign key.';
COMMENT ON COLUMN pgokf_private.sync_log.bundle_path IS
    'Canonical path (filesystem root or the content:<name> synthetic key) of the affected bundle, captured at operation time.';
COMMENT ON COLUMN pgokf_private.sync_log.op IS
    'The operation: register / refresh / content (register_bundle_content) / unregister.';
COMMENT ON COLUMN pgokf_private.sync_log.actor IS
    'The session_user that performed the operation, captured by column default.';
COMMENT ON COLUMN pgokf_private.sync_log.synced_at IS
    'When the operation committed (transaction now()); the column pruning compares against sync_log_retention_days.';
COMMENT ON COLUMN pgokf_private.sync_log.added IS
    'Count of concepts added by the sync; NULL for an unregister row.';
COMMENT ON COLUMN pgokf_private.sync_log.updated IS
    'Count of concepts updated by the sync; NULL for an unregister row.';
COMMENT ON COLUMN pgokf_private.sync_log.removed IS
    'Count of concepts removed by the sync; NULL for an unregister row.';
COMMENT ON COLUMN pgokf_private.sync_log.unchanged IS
    'Count of concepts left unchanged by the sync; NULL for an unregister row.';
COMMENT ON COLUMN pgokf_private.sync_log.total IS
    'Total files considered by the sync (added + updated + removed + unchanged); NULL for an unregister row.';
COMMENT ON COLUMN pgokf_private.sync_log.sync_hash IS
    'Aggregate BLAKE3 digest of the synced snapshot (matches pgokf.bundles.sync_hash); NULL for an unregister row.';

-- ===========================================================================
-- Step 3: new composite result types.
--
-- These match, attribute for attribute, the CREATE TYPE statements the fresh
-- 0.1.5 schema emits, so the C functions below resolve their return rowtypes
-- identically whether the extension was created at 0.1.5 or upgraded to it.
-- ===========================================================================
CREATE TYPE pgokf.sync_log_entry AS (
    id          bigint,
    bundle_id   bigint,
    bundle_path text,
    op          text,
    actor       text,
    synced_at   timestamptz,
    added       integer,
    updated     integer,
    removed     integer,
    unchanged   integer,
    total       integer
);

COMMENT ON TYPE pgokf.sync_log_entry IS
    'One audit-trail entry from pgokf.list_sync_log: the operation (register/refresh/content/unregister), who ran it, when it committed, and its per-bucket change counts (NULL for an unregister).';

CREATE TYPE pgokf.catalog_stat AS (
    bundle_id           bigint,
    name                text,
    enabled             boolean,
    source_type         text,
    file_count          integer,
    indexed_concepts    bigint,
    link_count          bigint,
    resolved_link_count bigint,
    last_synced_at      timestamptz,
    sync_age            interval,
    is_stale            boolean
);

COMMENT ON TYPE pgokf.catalog_stat IS
    'Per-bundle operational statistics from pgokf.catalog_stats: identity and state, indexed-concept / link / resolved-link counts, sync recency (last_synced_at, sync_age), and a 24-hour staleness flag.';

CREATE TYPE pgokf.stale_concept AS (
    bundle_id    bigint,
    concept_id   text,
    path         text,
    concept_type text,
    stale_after  timestamptz
);

COMMENT ON TYPE pgokf.stale_concept IS
    'One concept whose OKF stale_after instant has passed (as of the chosen time), from pgokf.stale_concepts: its bundle, id, path, type, and the stale_after instant.';

-- ===========================================================================
-- Step 4: the five new SQL-callable functions.
--
-- Each C symbol (<fn>_wrapper), its argument/return signature, and its
-- STRICT/STABLE/PARALLEL SAFE markers mirror exactly what the fresh 0.1.5 schema
-- emits from crates/extension/src/catalog/{audit,admin,stats}.rs, so an upgraded
-- catalog resolves and authorizes them byte-identically to a fresh install. The
-- 0.1.5 shared library exports every one of these symbols.
-- ===========================================================================

-- F1: the reader-level audit-log projection. SECURITY DEFINER because the log
-- table lives in the administrator-only pgokf_private schema.
CREATE FUNCTION pgokf."list_sync_log"(
    "bundle_id" bigint DEFAULT NULL,
    "max_rows" integer DEFAULT 100
) RETURNS SETOF pgokf.sync_log_entry
LANGUAGE c
AS 'MODULE_PATHNAME', 'list_sync_log_wrapper';

ALTER FUNCTION pgokf.list_sync_log(bigint, integer)
    SECURITY DEFINER SET search_path = pg_catalog, pg_temp;
REVOKE ALL ON FUNCTION pgokf.list_sync_log(bigint, integer) FROM PUBLIC;
GRANT EXECUTE ON FUNCTION pgokf.list_sync_log(bigint, integer) TO pgokf_reader;
COMMENT ON FUNCTION pgokf.list_sync_log(bigint, integer) IS
    'List recent pgokf_private.sync_log audit entries as pgokf.sync_log_entry, newest first, optionally scoped to one bundle and bounded by max_rows. Reader-level (SECURITY DEFINER over the admin-only log table); raises 22023 when max_rows < 0.';

-- F2: the writer-tier bundle enable/disable toggle.
CREATE FUNCTION pgokf."set_bundle_enabled"(
    "bundle_id" bigint,
    "enabled" boolean
) RETURNS pgokf.bundle_info
STRICT
LANGUAGE c
AS 'MODULE_PATHNAME', 'set_bundle_enabled_wrapper';

ALTER FUNCTION pgokf.set_bundle_enabled(bigint, boolean)
    SECURITY DEFINER SET search_path = pg_catalog, pg_temp;
REVOKE ALL ON FUNCTION pgokf.set_bundle_enabled(bigint, boolean) FROM PUBLIC;
GRANT EXECUTE ON FUNCTION pgokf.set_bundle_enabled(bigint, boolean) TO pgokf_writer;
COMMENT ON FUNCTION pgokf.set_bundle_enabled(bigint, boolean) IS
    'Enable or disable a registered bundle, returning the updated pgokf.bundle_info. Writer-tier (pgokf_writer; admin inherits it); a disabled bundle is hidden from concept_search and concept_neighbors without deleting rows (reversible). Raises 22023 if the bundle_id is unknown.';

-- F4: per-bundle statistics. Invoker rights (reads only reader-granted tables).
CREATE FUNCTION pgokf."catalog_stats"() RETURNS SETOF pgokf.catalog_stat
STRICT STABLE PARALLEL SAFE
LANGUAGE c
AS 'MODULE_PATHNAME', 'catalog_stats_wrapper';

REVOKE ALL ON FUNCTION pgokf.catalog_stats() FROM PUBLIC;
GRANT EXECUTE ON FUNCTION pgokf.catalog_stats() TO pgokf_reader;
COMMENT ON FUNCTION pgokf.catalog_stats() IS
    'Per-bundle operational statistics (indexed-concept/link/resolved-link counts, sync recency, 24h staleness flag) as pgokf.catalog_stat. Reader-level, STABLE, invoker rights over reader-granted tables.';

-- F5: the health document. SECURITY DEFINER because it reads pgokf_private.config.
CREATE FUNCTION pgokf."health"() RETURNS jsonb
STRICT STABLE
LANGUAGE c
AS 'MODULE_PATHNAME', 'health_wrapper';

ALTER FUNCTION pgokf.health()
    SECURITY DEFINER SET search_path = pg_catalog, pg_temp;
REVOKE ALL ON FUNCTION pgokf.health() FROM PUBLIC;
GRANT EXECUTE ON FUNCTION pgokf.health() TO pgokf_reader;
COMMENT ON FUNCTION pgokf.health() IS
    'Catalog health document (jsonb) for liveness/readiness probes: ok, bundle_count, concept_count, search_backend, bm25_ready, in_recovery, roles_ok, config_ok. Reader-level, STABLE, SECURITY DEFINER (reads the admin-only config).';

-- F6: concepts past their OKF stale_after. Invoker rights (reader-granted tables).
CREATE FUNCTION pgokf."stale_concepts"(
    "bundle_id" bigint DEFAULT NULL,
    "as_of" timestamp with time zone DEFAULT NULL
) RETURNS SETOF pgokf.stale_concept
STABLE PARALLEL SAFE
LANGUAGE c
AS 'MODULE_PATHNAME', 'stale_concepts_wrapper';

REVOKE ALL ON FUNCTION pgokf.stale_concepts(bigint, timestamptz) FROM PUBLIC;
GRANT EXECUTE ON FUNCTION pgokf.stale_concepts(bigint, timestamptz) TO pgokf_reader;
COMMENT ON FUNCTION pgokf.stale_concepts(bigint, timestamptz) IS
    'List concepts whose OKF stale_after instant has passed as of the given time (or now()), as pgokf.stale_concept, optionally scoped to one bundle. Reader-level, STABLE, invoker rights over reader-granted tables.';

-- ===========================================================================
-- Step 5: refresh the pgokf.concept_neighbors comment to the fresh-0.1.5 text.
--
-- F2 gave concept_neighbors the enabled-bundle filter (its .so behavior updates
-- automatically when the 0.1.5 module loads), and its source COMMENT was
-- reworded to match (over enabled bundles only). The function signature, C
-- symbol, and ACLs are unchanged, so only the comment needs refreshing here so
-- an upgraded catalog is byte-identical to a fresh 0.1.5 install.
-- ===========================================================================
COMMENT ON FUNCTION pgokf.concept_neighbors(text, integer, bigint) IS
    'Cycle-safe recursive traversal of resolved internal links from a concept, over enabled bundles only (matching concept_search). Reader-level; capped at pgokf.max_graph_hops. Raises 22023 on max_hops < 1 or an ambiguous concept_id.';
