-- pgokf extension upgrade: 0.1.15 -> 0.1.16
--
-- 0.1.16 adds the `require_tenant` durable policy key (default false). When
-- it is on, a session that has not set pgokf.tenant sees no rows through the
-- row-level-security policies and every reader built on them, the SECURITY
-- DEFINER readers report nothing for it, and the ingestion-tier and
-- bundle-addressed write functions refuse it with SQLSTATE 42501. With the
-- default, an unset session stays cross-tenant: the behavior every existing
-- install has today, so this upgrade changes nothing until an administrator
-- opts in with pgokf.set_config('require_tenant', 'true').
--
-- The script adds the column, creates the reader-level pgokf.tenant_required()
-- function the policies consult (an uncorrelated sub-select, evaluated once
-- per statement), rewrites all thirteen tenant-isolation policies in place
-- with ALTER POLICY (non-destructive: no row is touched, no policy is
-- dropped), replaces the ParadeDB bm25_hits helper so it applies the same
-- rule, and refreshes the comments whose text changed (schedule_refresh's
-- job command now pins the bundle's tenant; jobs scheduled by earlier
-- releases keep their bare command until re-scheduled). The Rust entry points
-- that enforce the write-side rule live in the shared library. A catalog
-- upgraded with this script is identical to a fresh 0.1.16 install.
--
-- Never DROP, TRUNCATE, DELETE, or rewrite existing catalog data in an upgrade
-- script: doing so would break the no-data-loss guarantee asserted by the
-- api_stability upgrade tests.

ALTER TABLE pgokf_private.config
    ADD COLUMN IF NOT EXISTS require_tenant boolean NOT NULL DEFAULT false;

COMMENT ON COLUMN pgokf_private.config.require_tenant IS
    'Whether the catalog requires an active tenant: when true, a session that has not set pgokf.tenant sees no rows through the row-level-security policies and every reader surface built on them, the SECURITY DEFINER readers apply the same rule (empty results; get_concept_source raises the same not-found error a foreign tenant gets), and the ingestion-tier and bundle-addressed write functions refuse it with SQLSTATE 42501 - including a pg_cron refresh job whose session carries no tenant, which is why schedule_refresh pins the bundle''s tenant into the job command; when false (the default) an unset session is cross-tenant - the backward-compatible see-all behavior. Read by pgokf.tenant_required(). Admin configuration (set_config/reset_config) never needs a tenant, so the policy can always be turned back off.';

-- The function every policy references. In a fresh install it is created by
-- the bootstrap, before any table; here the table already exists.
CREATE OR REPLACE FUNCTION pgokf.tenant_required() RETURNS boolean
    LANGUAGE plpgsql
    STABLE
    PARALLEL SAFE
    SECURITY DEFINER
    SET search_path = pg_catalog, pg_temp
    AS $fn$
BEGIN
    RETURN (SELECT require_tenant FROM pgokf_private.config WHERE singleton);
END
$fn$;
GRANT EXECUTE ON FUNCTION pgokf.tenant_required() TO PUBLIC;
COMMENT ON FUNCTION pgokf.tenant_required() IS
    'Whether the durable policy key require_tenant is on: when true, a session that has not set pgokf.tenant sees no catalog rows (every row-level-security policy consults this, and the SECURITY DEFINER readers apply the same rule: empty results, or for get_concept_source the same not-found error a foreign tenant gets) and the ingestion and bundle-addressed write functions refuse to run for it (SQLSTATE 42501); when false (the default) an unset session is cross-tenant, the backward-compatible see-all behavior. STABLE, executable by any role with USAGE on schema pgokf (the policies depend on it); a client can call it to decide whether it must scope its session before reading.';

-- Row-level-security policies: the same predicate on every tenant-isolated
-- table, now consulting the policy for an unscoped session.
ALTER POLICY bundles_tenant_isolation ON pgokf.bundles
    USING (((pg_catalog.current_setting('pgokf.tenant', true) IS NULL
             OR pg_catalog.current_setting('pgokf.tenant', true) = '')
            AND NOT (SELECT pgokf.tenant_required()))
        OR tenant_id = pg_catalog.current_setting('pgokf.tenant', true))
    WITH CHECK (((pg_catalog.current_setting('pgokf.tenant', true) IS NULL
                  OR pg_catalog.current_setting('pgokf.tenant', true) = '')
                 AND NOT (SELECT pgokf.tenant_required()))
        OR tenant_id = pg_catalog.current_setting('pgokf.tenant', true));

ALTER POLICY concepts_tenant_isolation ON pgokf.concepts
    USING (((pg_catalog.current_setting('pgokf.tenant', true) IS NULL
             OR pg_catalog.current_setting('pgokf.tenant', true) = '')
            AND NOT (SELECT pgokf.tenant_required()))
        OR tenant_id = pg_catalog.current_setting('pgokf.tenant', true))
    WITH CHECK (((pg_catalog.current_setting('pgokf.tenant', true) IS NULL
                  OR pg_catalog.current_setting('pgokf.tenant', true) = '')
                 AND NOT (SELECT pgokf.tenant_required()))
        OR tenant_id = pg_catalog.current_setting('pgokf.tenant', true));

ALTER POLICY concept_metadata_tenant_isolation ON pgokf.concept_metadata
    USING (((pg_catalog.current_setting('pgokf.tenant', true) IS NULL
             OR pg_catalog.current_setting('pgokf.tenant', true) = '')
            AND NOT (SELECT pgokf.tenant_required()))
        OR tenant_id = pg_catalog.current_setting('pgokf.tenant', true))
    WITH CHECK (((pg_catalog.current_setting('pgokf.tenant', true) IS NULL
                  OR pg_catalog.current_setting('pgokf.tenant', true) = '')
                 AND NOT (SELECT pgokf.tenant_required()))
        OR tenant_id = pg_catalog.current_setting('pgokf.tenant', true));

ALTER POLICY links_tenant_isolation ON pgokf.links
    USING (((pg_catalog.current_setting('pgokf.tenant', true) IS NULL
             OR pg_catalog.current_setting('pgokf.tenant', true) = '')
            AND NOT (SELECT pgokf.tenant_required()))
        OR tenant_id = pg_catalog.current_setting('pgokf.tenant', true))
    WITH CHECK (((pg_catalog.current_setting('pgokf.tenant', true) IS NULL
                  OR pg_catalog.current_setting('pgokf.tenant', true) = '')
                 AND NOT (SELECT pgokf.tenant_required()))
        OR tenant_id = pg_catalog.current_setting('pgokf.tenant', true));

ALTER POLICY concept_provenance_tenant_isolation ON pgokf.concept_provenance
    USING (((pg_catalog.current_setting('pgokf.tenant', true) IS NULL
             OR pg_catalog.current_setting('pgokf.tenant', true) = '')
            AND NOT (SELECT pgokf.tenant_required()))
        OR tenant_id = pg_catalog.current_setting('pgokf.tenant', true))
    WITH CHECK (((pg_catalog.current_setting('pgokf.tenant', true) IS NULL
                  OR pg_catalog.current_setting('pgokf.tenant', true) = '')
                 AND NOT (SELECT pgokf.tenant_required()))
        OR tenant_id = pg_catalog.current_setting('pgokf.tenant', true));

ALTER POLICY concept_verification_tenant_isolation ON pgokf.concept_verification
    USING (((pg_catalog.current_setting('pgokf.tenant', true) IS NULL
             OR pg_catalog.current_setting('pgokf.tenant', true) = '')
            AND NOT (SELECT pgokf.tenant_required()))
        OR tenant_id = pg_catalog.current_setting('pgokf.tenant', true))
    WITH CHECK (((pg_catalog.current_setting('pgokf.tenant', true) IS NULL
                  OR pg_catalog.current_setting('pgokf.tenant', true) = '')
                 AND NOT (SELECT pgokf.tenant_required()))
        OR tenant_id = pg_catalog.current_setting('pgokf.tenant', true));

ALTER POLICY concept_provenance_source_tenant_isolation ON pgokf.concept_provenance_source
    USING (((pg_catalog.current_setting('pgokf.tenant', true) IS NULL
             OR pg_catalog.current_setting('pgokf.tenant', true) = '')
            AND NOT (SELECT pgokf.tenant_required()))
        OR tenant_id = pg_catalog.current_setting('pgokf.tenant', true))
    WITH CHECK (((pg_catalog.current_setting('pgokf.tenant', true) IS NULL
                  OR pg_catalog.current_setting('pgokf.tenant', true) = '')
                 AND NOT (SELECT pgokf.tenant_required()))
        OR tenant_id = pg_catalog.current_setting('pgokf.tenant', true));

ALTER POLICY concept_embedding_tenant_isolation ON pgokf.concept_embedding
    USING (((pg_catalog.current_setting('pgokf.tenant', true) IS NULL
             OR pg_catalog.current_setting('pgokf.tenant', true) = '')
            AND NOT (SELECT pgokf.tenant_required()))
        OR tenant_id = pg_catalog.current_setting('pgokf.tenant', true))
    WITH CHECK (((pg_catalog.current_setting('pgokf.tenant', true) IS NULL
                  OR pg_catalog.current_setting('pgokf.tenant', true) = '')
                 AND NOT (SELECT pgokf.tenant_required()))
        OR tenant_id = pg_catalog.current_setting('pgokf.tenant', true));

ALTER POLICY concept_source_tenant_isolation ON pgokf.concept_source
    USING (((pg_catalog.current_setting('pgokf.tenant', true) IS NULL
             OR pg_catalog.current_setting('pgokf.tenant', true) = '')
            AND NOT (SELECT pgokf.tenant_required()))
        OR tenant_id = pg_catalog.current_setting('pgokf.tenant', true))
    WITH CHECK (((pg_catalog.current_setting('pgokf.tenant', true) IS NULL
                  OR pg_catalog.current_setting('pgokf.tenant', true) = '')
                 AND NOT (SELECT pgokf.tenant_required()))
        OR tenant_id = pg_catalog.current_setting('pgokf.tenant', true));

ALTER POLICY concept_history_tenant_isolation ON pgokf.concept_history
    USING (((pg_catalog.current_setting('pgokf.tenant', true) IS NULL
             OR pg_catalog.current_setting('pgokf.tenant', true) = '')
            AND NOT (SELECT pgokf.tenant_required()))
        OR tenant_id = pg_catalog.current_setting('pgokf.tenant', true))
    WITH CHECK (((pg_catalog.current_setting('pgokf.tenant', true) IS NULL
                  OR pg_catalog.current_setting('pgokf.tenant', true) = '')
                 AND NOT (SELECT pgokf.tenant_required()))
        OR tenant_id = pg_catalog.current_setting('pgokf.tenant', true));

ALTER POLICY access_log_tenant_isolation ON pgokf_private.access_log
    USING (((pg_catalog.current_setting('pgokf.tenant', true) IS NULL
             OR pg_catalog.current_setting('pgokf.tenant', true) = '')
            AND NOT (SELECT pgokf.tenant_required()))
        OR tenant_id = pg_catalog.current_setting('pgokf.tenant', true))
    WITH CHECK (((pg_catalog.current_setting('pgokf.tenant', true) IS NULL
                  OR pg_catalog.current_setting('pgokf.tenant', true) = '')
                 AND NOT (SELECT pgokf.tenant_required()))
        OR tenant_id = pg_catalog.current_setting('pgokf.tenant', true));

ALTER POLICY sync_log_change_tenant_isolation ON pgokf_private.sync_log_change
    USING (((pg_catalog.current_setting('pgokf.tenant', true) IS NULL
             OR pg_catalog.current_setting('pgokf.tenant', true) = '')
            AND NOT (SELECT pgokf.tenant_required()))
        OR tenant_id = pg_catalog.current_setting('pgokf.tenant', true))
    WITH CHECK (((pg_catalog.current_setting('pgokf.tenant', true) IS NULL
                  OR pg_catalog.current_setting('pgokf.tenant', true) = '')
                 AND NOT (SELECT pgokf.tenant_required()))
        OR tenant_id = pg_catalog.current_setting('pgokf.tenant', true));

ALTER POLICY bundle_log_tenant_isolation ON pgokf.bundle_log
    USING (((pg_catalog.current_setting('pgokf.tenant', true) IS NULL
             OR pg_catalog.current_setting('pgokf.tenant', true) = '')
            AND NOT (SELECT pgokf.tenant_required()))
        OR tenant_id = pg_catalog.current_setting('pgokf.tenant', true))
    WITH CHECK (((pg_catalog.current_setting('pgokf.tenant', true) IS NULL
                  OR pg_catalog.current_setting('pgokf.tenant', true) = '')
                 AND NOT (SELECT pgokf.tenant_required()))
        OR tenant_id = pg_catalog.current_setting('pgokf.tenant', true));

-- The ParadeDB helper bypasses the policies (SECURITY DEFINER) and applies
-- their rule itself; replaced with the 0.1.16 body.
CREATE OR REPLACE FUNCTION pgokf.bm25_hits(
    p_query text,
    p_bundle_id bigint,
    p_limit bigint,
    p_text_search_config text,
    p_concept_type text,
    p_tags text[],
    p_status text,
    p_trust_tier text,
    p_after_rank real,
    p_after_bundle_id bigint,
    p_after_concept_id text)
RETURNS TABLE (
    bundle_id bigint,
    concept_id text,
    path text,
    title text,
    type text,
    rank real,
    headline text)
LANGUAGE plpgsql
STABLE PARALLEL SAFE
SECURITY DEFINER
SET search_path = pg_catalog, pg_temp
AS $fn$
DECLARE
    -- Resolved once, up front, and bound into the query as a parameter: the
    -- policies inline current_setting() directly, but pg_search 0.25 cannot
    -- plan its scan under a predicate that calls a function (that inline
    -- form is exactly what row-level security injects for non-owners, and
    -- exactly what the Unsupported-query-shape error was about). Empty = unset.
    v_tenant text := NULLIF(pg_catalog.current_setting('pgokf.tenant', true), '');
BEGIN
    -- The policies' rule, applied here because this body bypasses them: an
    -- unscoped session sees nothing when the catalog requires a tenant.
    IF v_tenant IS NULL AND pgokf.tenant_required() THEN
        RETURN;
    END IF;
    RETURN QUERY
    SELECT hits.bundle_id,
           hits.concept_id,
           hits.path,
           hits.title,
           hits.type,
           hits.rank,
           hits.headline
    FROM (
        SELECT c.bundle_id AS bundle_id,
               c.id AS concept_id,
               c.path AS path,
               c.title AS title,
               c.type AS type,
               paradedb.score(c) AS rank,
               pg_catalog.ts_headline(
                   p_text_search_config::pg_catalog.regconfig,
                   pg_catalog.concat_ws(' ', c.title, c.description, c.body_text),
                   pg_catalog.websearch_to_tsquery(p_text_search_config::pg_catalog.regconfig, p_query)) AS headline
        FROM pgokf.concepts c
        JOIN pgokf.bundles b ON b.id = c.bundle_id AND b.enabled AND b.retired_at IS NULL
        LEFT JOIN pgokf.concept_provenance cp
               ON cp.bundle_id = c.bundle_id AND cp.concept_id = c.id
        WHERE c.id @@@ paradedb.boolean(should => ARRAY[
                  paradedb.match('title', p_query),
                  paradedb.match('description', p_query),
                  paradedb.match('body_text', p_query)])
          AND (v_tenant IS NULL OR c.tenant_id = v_tenant)
          AND (p_bundle_id IS NULL OR c.bundle_id = p_bundle_id)
          AND (p_concept_type IS NULL OR c.type = p_concept_type)
          AND (p_tags IS NULL OR c.tags @> p_tags)
          AND (p_status IS NULL OR cp.status = p_status)
          AND (p_trust_tier IS NULL OR cp.trust_tier = p_trust_tier)
    ) AS hits
    WHERE p_after_rank IS NULL
       OR hits.rank < p_after_rank
       OR (hits.rank = p_after_rank AND hits.bundle_id > p_after_bundle_id)
       OR (hits.rank = p_after_rank AND hits.bundle_id = p_after_bundle_id AND hits.concept_id > p_after_concept_id)
    ORDER BY hits.rank DESC, hits.bundle_id ASC, hits.concept_id ASC
    LIMIT p_limit;
END
$fn$;

COMMENT ON FUNCTION pgokf.health() IS
    'Catalog health document (jsonb) for liveness/readiness probes: ok, bundle_count, concept_count, search_backend, bm25_ready, tenant_required (the require_tenant policy; the two counts are empty for an unscoped session when it is on), in_recovery, roles_ok, config_ok. Reader-level, STABLE, SECURITY DEFINER (reads the admin-only config).';

COMMENT ON FUNCTION pgokf.schedule_refresh(bigint, text) IS
    'Schedule a recurring pgokf.refresh_bundle(<bundle_id>) via pg_cron under the deterministic job name pgokf_refresh_<bundle_id> (idempotent/re-schedulable), returning the job name. Admin-only, SECURITY DEFINER, tenant-confined. The scheduled command pins the bundle''s tenant (set_config(''pgokf.tenant'', <tenant_id>, false), quoted by format(%L)) and then runs SELECT pgokf.refresh_bundle(<id>) with the id as a trusted integer literal, so the cron worker''s own session satisfies the tenant rules and the require_tenant policy; the schedule and job name bind as parameters. Requires pg_cron: raises 22023 naming the missing dependency when it is not installed (no silent success), and 22023 for an unknown/cross-tenant bundle_id or an empty/oversized schedule. Full scheduling requires pg_cron in shared_preload_libraries.';

-- Last, so any relation a script adds is registered for pg_dump (none here;
-- the rule holds for every upgrade script).
SELECT pgokf_private.register_dump_relations();
