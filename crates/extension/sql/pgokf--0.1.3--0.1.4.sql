-- pgokf extension upgrade: 0.1.3 -> 0.1.4
--
-- This upgrade carries the COMPLETE delta from the tagged v0.1.3 release
-- (commit b51541e, which shipped only pgokf_reader / pgokf_admin) up to a fresh
-- 0.1.4 install, so that `ALTER EXTENSION pgokf UPDATE TO '0.1.4'` yields a
-- catalog functionally identical to `CREATE EXTENSION pgokf` at 0.1.4. It folds
-- in two feature waves:
--
--   Wave 1 (writer tier + search-index rebuild):
--     * introduces the pgokf_writer role — a least-privilege tier between
--       pgokf_reader and pgokf_admin — and wires it EXACTLY as bootstrap.sql
--       does (schema USAGE, reader < writer < admin inheritance, role comment).
--       Roles are cluster-global and bootstrap.sql does NOT re-run on
--       `ALTER EXTENSION UPDATE`, so the role and its grants MUST be (re)made
--       here or ingestion breaks: the SECURITY DEFINER sync functions authorize
--       Operation::Ingest against pgokf_writer membership, and a missing role
--       means nobody can ingest.
--     * moves register_bundle / refresh_bundle / unregister_bundle from the
--       admin tier (their 0.1.3 grantee) to the writer tier, with admin keeping
--       EXECUTE by inheriting pgokf_writer — matching the fresh-0.1.4 ACL, and
--       refreshes their comments to the writer-tier wording fresh 0.1.4 ships.
--     * adds pgokf.rebuild_search_index() (admin-only, SECURITY DEFINER) to
--       (re)build the optional ParadeDB pg_search bm25 index on pgokf.concepts,
--       plus the pgokf_private.config.search_backend policy column
--       ('native' | 'bm25', default 'native') that selects the backend, and
--       refreshes the concept_search comment to the bm25-aware fresh wording.
--
--   Wave 2 (mountless object-store ingestion):
--     * adds pgokf.bundles.source_type, distinguishing a 'filesystem' bundle
--       (register_bundle / refresh_bundle, canonical on-disk root) from a
--       'content' bundle (register_bundle_content, keyed on content:<name>);
--     * creates pgokf.register_bundle_content(text, text[], bytea[], jsonb),
--       writer-tier and SECURITY DEFINER, mirroring the register/refresh
--       hardening. A standalone companion process (the pgokf-ingest crate)
--       reads an S3-compatible store and streams the collected (path, bytes)
--       into PostgreSQL through it, so the extension still performs no network
--       I/O.
--
-- Every statement is idempotent and non-destructive: the writer role is created
-- only when absent, GRANT ... TO ROLE is naturally re-runnable, existing bundles
-- keep source_type = 'filesystem' (the column default), and no row or object is
-- ever dropped. `ALTER EXTENSION pgokf UPDATE TO '0.1.4'` runs the whole file in
-- a single transaction.

-- ===========================================================================
-- Wave 1, step 1: the pgokf_writer role and its grants.
--
-- Mirrors crates/extension/sql/bootstrap.sql. Roles are cluster-wide shared
-- objects that bootstrap.sql only emits on CREATE EXTENSION, so an install
-- upgraded from the tagged 0.1.3 (reader + admin only) would otherwise be left
-- without pgokf_writer. Creating it here MUST precede the writer-tier function
-- grants below (including the Wave 2 register_bundle_content grant).
-- ===========================================================================
DO $pgokf_writer_role$
BEGIN
    IF NOT EXISTS (SELECT 1 FROM pg_catalog.pg_roles WHERE rolname = 'pgokf_writer') THEN
        CREATE ROLE pgokf_writer NOLOGIN;
    END IF;
END
$pgokf_writer_role$;

-- Every tier needs to reach the public API schema to run its functions.
GRANT USAGE ON SCHEMA pgokf TO pgokf_writer;

-- Least-privilege hierarchy reader < writer < admin: granting the lower role to
-- the higher one makes each tier inherit everything below it, so a writer can
-- also search and an admin can also ingest and search. pg_has_role resolves this
-- chain, so the in-function membership checks accept a higher tier wherever a
-- lower one is required.
GRANT pgokf_reader TO pgokf_writer;
GRANT pgokf_writer TO pgokf_admin;

-- Cluster-wide role comment, matching bootstrap.sql (lives in pg_shdescription).
COMMENT ON ROLE pgokf_writer IS
    'pgokf ingestion API role: may register, refresh, and unregister bundles; inherits pgokf_reader. Intended account for an automated ingestion pipeline / the content-ingestion API. Does not include configuration or file-writing exports.';

-- ===========================================================================
-- Wave 1, step 2: move register_bundle / refresh_bundle / unregister_bundle
-- from the admin tier to the writer tier.
--
-- In tagged 0.1.3 these three were granted to pgokf_admin only. Fresh 0.1.4
-- grants them to pgokf_writer, with admin retaining EXECUTE by inheriting
-- pgokf_writer (granted just above). Revoking the now-redundant direct
-- pgokf_admin grant matches the fresh-0.1.4 ACL exactly without ever removing
-- admin capability — REVOKE withdraws a privilege, not data or an object, and
-- admin still resolves EXECUTE through role inheritance.
-- ===========================================================================
GRANT EXECUTE ON FUNCTION pgokf.register_bundle(text, text, jsonb) TO pgokf_writer;
GRANT EXECUTE ON FUNCTION pgokf.refresh_bundle(bigint) TO pgokf_writer;
GRANT EXECUTE ON FUNCTION pgokf.unregister_bundle(bigint) TO pgokf_writer;

REVOKE EXECUTE ON FUNCTION pgokf.register_bundle(text, text, jsonb) FROM pgokf_admin;
REVOKE EXECUTE ON FUNCTION pgokf.refresh_bundle(bigint) FROM pgokf_admin;
REVOKE EXECUTE ON FUNCTION pgokf.unregister_bundle(bigint) FROM pgokf_admin;

-- Refresh the comments to the writer-tier wording fresh 0.1.4 emits, so the
-- upgraded pg_description matches a fresh install exactly.
COMMENT ON FUNCTION pgokf.register_bundle(text, text, jsonb) IS
    'Register an OKF bundle root and synchronize it into the catalog. Writer-tier (pgokf_writer; admin inherits it); raises 23505 if the canonical path is already registered.';
COMMENT ON FUNCTION pgokf.refresh_bundle(bigint) IS
    'Incrementally re-synchronize a registered bundle: re-parses only content-changed files, removes rows for deleted files. Writer-tier (pgokf_writer; admin inherits it).';
COMMENT ON FUNCTION pgokf.unregister_bundle(bigint) IS
    'Unregister a bundle and return the removed bundle_info. Writer-tier (pgokf_writer; admin inherits it); concept/metadata/feature rows cascade. Raises 22023 if the bundle_id is unknown.';

-- ===========================================================================
-- Wave 1, step 3: pgokf.rebuild_search_index() — admin-only (re)build of the
-- optional ParadeDB pg_search bm25 index on pgokf.concepts.
--
-- The C symbol (rebuild_search_index_wrapper), RETURNS bool, the STRICT marker,
-- and the SECURITY DEFINER / REVOKE / GRANT / COMMENT hardening all mirror what
-- the fresh 0.1.4 schema emits from
-- crates/extension/src/catalog/search_backend.rs, so an upgraded catalog
-- resolves and authorizes the function byte-identically to a fresh install. The
-- 0.1.4 shared library exports this symbol (0.1.4 only added symbols over
-- 0.1.3, so every 0.1.3 symbol still resolves too).
-- ===========================================================================
CREATE FUNCTION pgokf."rebuild_search_index"() RETURNS bool
STRICT
LANGUAGE c
AS 'MODULE_PATHNAME', 'rebuild_search_index_wrapper';

ALTER FUNCTION pgokf.rebuild_search_index()
    SECURITY DEFINER SET search_path = pg_catalog, pg_temp;
REVOKE ALL ON FUNCTION pgokf.rebuild_search_index() FROM PUBLIC;
GRANT EXECUTE ON FUNCTION pgokf.rebuild_search_index() TO pgokf_admin;
COMMENT ON FUNCTION pgokf.rebuild_search_index() IS
    'Admin-only. (Re)build the ParadeDB pg_search bm25 index on pgokf.concepts used by search_backend=bm25; returns true when built, or false (with a NOTICE) when pg_search is not installed.';

-- ===========================================================================
-- Wave 1, step 4: the search_backend policy column and the concept_search
-- comment refresh.
--
-- Fresh 0.1.4 ships pgokf_private.config.search_backend, the durable policy
-- pgokf.concept_search reads to pick its execution strategy (validated against
-- 'native' | 'bm25'). The 0.1.3 config table lacks it, so add it here with the
-- same NOT NULL default 'native' — backfilling the existing singleton row
-- non-destructively — plus the guarded CHECK and column comment. Also refresh
-- the concept_search comment to the bm25-aware fresh wording.
-- ===========================================================================
ALTER TABLE pgokf_private.config
    ADD COLUMN IF NOT EXISTS search_backend text NOT NULL DEFAULT 'native';

-- The CHECK is added separately (guarded) so re-running the step is idempotent.
DO $pgokf_search_backend_chk$
BEGIN
    ALTER TABLE pgokf_private.config
        ADD CONSTRAINT config_search_backend_chk
        CHECK (search_backend IN ('native', 'bm25'));
EXCEPTION WHEN duplicate_object THEN
    NULL;
END
$pgokf_search_backend_chk$;

COMMENT ON COLUMN pgokf_private.config.search_backend IS
    'Ranked-search execution backend for pgokf.concept_search: ''native'' (the default, zero-dependency PostgreSQL FTS available on every supported server) or ''bm25'' (Block-Max WAND top-k via the external ParadeDB pg_search extension). When set to ''bm25'' the search transparently falls back to native, with a warning, if pg_search is not installed or no bm25 index exists on pgokf.concepts (build one with pgokf.rebuild_search_index).';

COMMENT ON FUNCTION pgokf.concept_search(text, bigint, integer) IS
    'Rank catalog concepts. Reader-level; searches enabled bundles only. Uses the search_backend configuration: native full-text search (websearch_to_tsquery + ts_rank_cd) by default, or ParadeDB pg_search BM25 when set to bm25 (falling back to native if pg_search or its index is absent).';

-- ===========================================================================
-- Wave 2, step 1: distinguish filesystem-sourced from content-sourced bundles.
-- ===========================================================================
ALTER TABLE pgokf.bundles
    ADD COLUMN IF NOT EXISTS source_type text NOT NULL DEFAULT 'filesystem';

-- The CHECK is added separately (guarded) so re-running the step is idempotent.
DO $pgokf_source_type_chk$
BEGIN
    ALTER TABLE pgokf.bundles
        ADD CONSTRAINT bundles_source_type_chk
        CHECK (source_type IN ('filesystem', 'content'));
EXCEPTION WHEN duplicate_object THEN
    NULL;
END
$pgokf_source_type_chk$;

COMMENT ON COLUMN pgokf.bundles.source_type IS
    'How the bundle bytes reach the catalog: ''filesystem'' (registered from a canonical on-disk root via pgokf.register_bundle and refreshed from disk via pgokf.refresh_bundle) or ''content'' (streamed in memory via pgokf.register_bundle_content — a mountless object-store companion or any client — where path is the synthetic key ''content:''||name and refresh_bundle is rejected).';

-- ===========================================================================
-- Wave 2, step 2: the mountless content-ingestion entry point. The C symbol
-- matches the one pgrx generates for the freshly installed 0.1.4 schema, so a
-- bundle ingested through it behaves identically whether the extension was
-- created at 0.1.4 or upgraded to it.
-- ===========================================================================
CREATE FUNCTION pgokf."register_bundle_content"(
	"name" TEXT,
	"paths" TEXT[],
	"contents" bytea[],
	"options" jsonb DEFAULT '{}'
) RETURNS pgokf.bundle_sync_result
LANGUAGE c
AS 'MODULE_PATHNAME', 'register_bundle_content_wrapper';

ALTER FUNCTION pgokf.register_bundle_content(text, text[], bytea[], jsonb)
    SECURITY DEFINER SET search_path = pg_catalog, pg_temp;
REVOKE ALL ON FUNCTION pgokf.register_bundle_content(text, text[], bytea[], jsonb) FROM PUBLIC;
GRANT EXECUTE ON FUNCTION pgokf.register_bundle_content(text, text[], bytea[], jsonb) TO pgokf_writer;
COMMENT ON FUNCTION pgokf.register_bundle_content(text, text[], bytea[], jsonb) IS
    'Register or resync an OKF bundle from in-memory content: the mountless ingestion path a companion process uses to stream bytes read from an object store, so the extension performs no network I/O. paths[] and contents[] must be equal-length, non-null arrays of safe bundle-relative paths and their bytes; the bundle is keyed on content:<name> with source_type=''content'' and re-called to resync (changed concepts upserted, missing ones deleted). Writer-tier (pgokf_writer; admin inherits it). Raises 22023 on a shape/path violation, honoring the max_bundle_files/max_file_bytes ceilings.';
