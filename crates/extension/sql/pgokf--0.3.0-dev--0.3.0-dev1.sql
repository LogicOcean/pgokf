-- SPDX-License-Identifier: AGPL-3.0-only
-- pgokf extension upgrade: 0.3.0-dev -> 0.3.0-dev1
--
-- The first point-versioned step of the 0.3.0 development cycle (see the
-- "Development point versions" section of docs/api-stability.md). 0.3.0-dev1
-- adds no object beyond what main already defines; it receipts, through the
-- normal ALTER EXTENSION pgokf UPDATE machinery, the objects that entered the
-- cycle after the first 0.3.0-dev deployments and therefore never had an
-- update target:
--
--   * the scheduled-refresh read surface pgokf.list_scheduled_refreshes (the
--     scheduled_refreshes_reader block of src/catalog/schedule.rs): the
--     tenant-confined, SECURITY DEFINER listing of the pg_cron jobs
--     pgokf.schedule_refresh registers under the extension owner's identity;
--   * the external repository-registry surface (the registry_surface block of
--     src/catalog/registry.rs): the guarded column-level SELECT grant that
--     lets pgokf_reader list the producer service's
--     ast_graph.repository_registry where that schema shares the database,
--     and the admin-tier SECURITY DEFINER writers pgokf.registry_set_status /
--     pgokf.registry_set_poll_interval, both runtime-only couplings that
--     raise a curated 22023 where the producer schema is absent.
--
-- A 0.3.0-dev installation exists in the field in more than one state -
-- deployed before these objects entered the cycle, deployed after (the
-- 0.2.0 -> 0.3.0-dev script already carries them), or patched by hand from
-- the install file - so every statement here is idempotent. The functions
-- are re-created with CREATE OR REPLACE (same identity, same body: a no-op
-- where they already exist), the grants and comments re-apply cleanly, and
-- section 1 first adopts into the extension (ALTER EXTENSION ... ADD)
-- exactly those copies that were applied by hand outside the update
-- machinery and so carry no pg_depend membership. The order is load-bearing:
-- PostgreSQL refuses CREATE OR REPLACE on a non-member object during an
-- extension update ("an extension is not allowed to replace an object that
-- it does not own"), so adoption must precede re-creation; objects the
-- extension itself created are already members and are skipped, and where an
-- object is absent the CREATE below makes it a member directly. An
-- installation that reached 0.3.0-dev by any route is byte-identical to a
-- fresh 0.3.0-dev1 install after this script runs.
--
-- Never DROP, TRUNCATE, DELETE, or rewrite existing catalog data in an upgrade
-- script: doing so would break the no-data-loss guarantee asserted by the
-- api_stability upgrade tests.

-- ===========================================================================
-- 1. Extension-membership repair, BEFORE any CREATE OR REPLACE (see the
-- header). Objects this script creates become extension members
-- automatically, as does anything an earlier upgrade script created. A copy
-- applied by hand from the install file - the unreceipted deployment this
-- point version replaces - carries no pg_depend entry and must be adopted
-- first, or the re-creation below is refused.
-- ===========================================================================
DO $dev1_membership$
DECLARE
    v_signature text;
BEGIN
    FOR v_signature IN
        SELECT * FROM (VALUES
            ('pgokf.list_scheduled_refreshes()'),
            ('pgokf.registry_set_status(uuid, text)'),
            ('pgokf.registry_set_poll_interval(uuid, integer)')
        ) AS signatures(signature)
    LOOP
        IF pg_catalog.to_regprocedure(v_signature) IS NOT NULL
           AND NOT EXISTS (
            SELECT 1
            FROM pg_catalog.pg_depend AS d
            JOIN pg_catalog.pg_extension AS e ON e.oid = d.refobjid
            WHERE e.extname = 'pgokf'
              AND d.classid = 'pg_catalog.pg_proc'::pg_catalog.regclass
              AND d.objid = v_signature::pg_catalog.regprocedure
        ) THEN
            EXECUTE pg_catalog.format('ALTER EXTENSION pgokf ADD FUNCTION %s', v_signature);
        END IF;
    END LOOP;
END
$dev1_membership$;

-- ===========================================================================
-- 2. The scheduled-refresh read surface (the scheduled_refreshes_reader
-- block of src/catalog/schedule.rs, verbatim except for CREATE OR REPLACE -
-- see the header). pg_cron grants SELECT on cron.job to PUBLIC but restricts
-- rows to username = current_user, and pgokf.schedule_refresh - SECURITY
-- DEFINER since 0.1.9 - registers every job under the extension owner's
-- identity, so an ordinary login reading cron.job directly sees none of them.
-- pgokf.list_scheduled_refreshes runs as that owner (SECURITY DEFINER),
-- confines itself to the session tenant like the RLS-backed readers, and
-- raises the same 22023 schedule_refresh raises when pg_cron is not
-- installed. Reader-tier.
-- ===========================================================================
CREATE OR REPLACE FUNCTION pgokf.list_scheduled_refreshes()
RETURNS TABLE (bundle_id bigint, schedule text)
LANGUAGE plpgsql STABLE
SECURITY DEFINER SET search_path = pg_catalog, pg_temp
AS $list_scheduled_refreshes$
DECLARE
    v_tenant text := NULLIF(pg_catalog.current_setting('pgokf.tenant', true), '');
BEGIN
    -- The read-side tenant rule, applied explicitly because this body runs
    -- as the extension owner and so bypasses row-level security: an
    -- unscoped session sees nothing when the catalog requires a tenant,
    -- exactly like the RLS-backed readers.
    IF v_tenant IS NULL AND pgokf.tenant_required() THEN
        RETURN;
    END IF;
    -- Reading the jobs needs pg_cron exactly like scheduling them: refuse
    -- with the same 22023 naming the missing dependency rather than
    -- failing on the absent cron.job relation.
    IF NOT (SELECT pg_catalog.count(*) > 0
            FROM pg_catalog.pg_extension
            WHERE extname = 'pg_cron') THEN
        RAISE EXCEPTION
            'listing scheduled refreshes requires the pg_cron extension, which is not installed; add pg_cron to shared_preload_libraries and run CREATE EXTENSION pg_cron'
            USING ERRCODE = 'invalid_parameter_value';
    END IF;
    -- SECURITY DEFINER is the point of this surface: pg_cron grants SELECT
    -- on cron.job to PUBLIC but its row-security policy restricts rows to
    -- username = current_user, and pgokf.schedule_refresh - itself
    -- SECURITY DEFINER - registers every job under the extension owner's
    -- identity. An ordinary login therefore reads zero rows whatever its
    -- grants; running as that owner, this read sees exactly the jobs the
    -- extension manages. (A job another role created directly under the
    -- convention's name is pg_cron-invisible to it, by the same policy.)
    RETURN QUERY
    SELECT b.id, j.schedule
    FROM cron.job AS j
    JOIN pgokf.bundles AS b
      ON b.id = pg_catalog.substring(j.jobname, 'pgokf_refresh_([0-9]{1,18})')::bigint
    WHERE j.jobname ~ '^pgokf_refresh_[0-9]{1,18}$'
      AND (v_tenant IS NULL OR b.tenant_id = v_tenant)
    ORDER BY b.id;
END
$list_scheduled_refreshes$;
REVOKE ALL ON FUNCTION pgokf.list_scheduled_refreshes() FROM PUBLIC;
GRANT EXECUTE ON FUNCTION pgokf.list_scheduled_refreshes() TO pgokf_reader;
COMMENT ON FUNCTION pgokf.list_scheduled_refreshes() IS
    'Every scheduled bundle refresh the extension manages, as (bundle_id, schedule) rows ordered by bundle id: the read counterpart of pgokf.schedule_refresh / unschedule_refresh. SECURITY DEFINER because pg_cron restricts cron.job rows to username = current_user (grants do not change that) while schedule_refresh registers jobs under the extension owner''s identity - this read runs as that owner, so the app''s login sees the schedules it would otherwise read as zero rows. Tenant-confined like the RLS-backed readers (an unscoped session sees nothing when require_tenant is on; a scoped session sees only its tenant''s jobs), joining each pgokf_refresh_<id> job back to its bundle. Requires pg_cron: raises 22023 naming the missing dependency when it is not installed, exactly like schedule_refresh. Reader-tier (granted to pgokf_reader, inherited by writer and admin).';

-- ===========================================================================
-- 3. The external repository-registry surface (the registry_surface block of
-- src/catalog/registry.rs, verbatim except for CREATE OR REPLACE - see the
-- header). A registered repository's row is owned by the repository-registry
-- producer service, a separate codebase whose migrations create the ast_graph
-- schema; the coupling is runtime-only, exactly like the pg_cron adapter.
-- Reads follow the narrow grant pattern: pgokf_reader gets USAGE on the
-- ast_graph schema and SELECT on exactly the columns the admin UI lists
-- (never checkout_path or the producer's internal graph_id, and no secret
-- exists here at all - fetch credentials live behind the producer's admin
-- API, which never returns them; tenant_id is granted so callers can confine
-- their read to the session tenant), applied only where the table is
-- present. Writes go through the SECURITY DEFINER functions below, granted
-- to pgokf_admin; each resolves the table at call time, raises a curated
-- 22023 where it is absent, and confines its update to the session tenant
-- (pgokf.tenant), so a cross-tenant id earns the same 22023 as an unknown
-- one.
-- ===========================================================================

-- The reader grant applies only where the producer's registry table exists.
-- Roles are cluster-wide and the table belongs to another service's schema,
-- so neither can be an extension member; a guarded DO block keeps
-- CREATE EXTENSION pgokf working in databases the producer does not share.
DO $registry_reader_grant$
BEGIN
    IF pg_catalog.to_regclass('ast_graph.repository_registry') IS NOT NULL THEN
        GRANT USAGE ON SCHEMA ast_graph TO pgokf_reader;
        GRANT SELECT (repository_id, repository_key, project_name, default_branch,
                      remote_url, status, poll_interval_seconds,
                      last_indexed_commit, last_published_commit,
                      last_published_generation, tenant_id)
            ON ast_graph.repository_registry TO pgokf_reader;
    ELSE
        RAISE NOTICE 'pgokf: ast_graph.repository_registry is not present in this database; skipping the registry reader grant (the producer service installs that schema)';
    END IF;
END
$registry_reader_grant$;

CREATE OR REPLACE FUNCTION pgokf.registry_set_status(repository_id uuid, status text)
RETURNS void
LANGUAGE plpgsql
VOLATILE
SECURITY DEFINER
SET search_path = pg_catalog, pg_temp
AS $registry_set_status$
BEGIN
    -- Late binding is the point: the table reference below is planned on
    -- first execution, so this curated 22023 answers instead of a bare
    -- 42P01 in a database the producer does not share.
    IF pg_catalog.to_regclass('ast_graph.repository_registry') IS NULL THEN
        RAISE EXCEPTION
            'registry writes require the producer service schema (ast_graph.repository_registry), which is not present in this database'
            USING ERRCODE = 'invalid_parameter_value';
    END IF;
    -- The producer's poll loop enumerates status 'active' rows only, so
    -- 'paused' stops a repository's reconciliation without deleting
    -- anything; any other value is refused rather than inventing a state
    -- the producer does not define.
    IF status NOT IN ('active', 'paused') THEN
        RAISE EXCEPTION
            'registry status must be active or paused, not %', status
            USING ERRCODE = 'invalid_parameter_value';
    END IF;
    UPDATE ast_graph.repository_registry AS r
       SET status = registry_set_status.status,
           updated_at = pg_catalog.now()
     WHERE r.repository_id = registry_set_status.repository_id
       -- The session tenant confines the write: a cross-tenant id finds no
       -- row and earns the same 22023 an unknown id does, so the answer
       -- never reveals that another tenant's repository exists. An unset,
       -- empty (the GUC's registered default), or all-whitespace
       -- pgokf.tenant all normalize to the producer's 'default' tenant.
       AND r.tenant_id = COALESCE(NULLIF(pg_catalog.btrim(pg_catalog.current_setting('pgokf.tenant', true)), ''), 'default');
    IF NOT FOUND THEN
        RAISE EXCEPTION
            'no registered repository with id %', repository_id
            USING ERRCODE = 'invalid_parameter_value';
    END IF;
END
$registry_set_status$;
REVOKE ALL ON FUNCTION pgokf.registry_set_status(uuid, text) FROM PUBLIC;
GRANT EXECUTE ON FUNCTION pgokf.registry_set_status(uuid, text) TO pgokf_admin;
COMMENT ON FUNCTION pgokf.registry_set_status(uuid, text) IS
    'Pause or resume one registered repository of the external repository-registry producer service by setting its ast_graph.repository_registry status (active or paused; the producer polls active rows only, so pausing stops reconciliation without deleting the registration). Admin-only (pgokf_admin), SECURITY DEFINER over a table no API role may write directly; tenant-confined: the update matches only rows whose tenant_id equals the session''s pgokf.tenant setting, with an unset, empty (the GUC''s registered default), or all-whitespace value normalizing to the producer''s ''default'' tenant, so a cross-tenant id earns the same 22023 as an unknown one without revealing that the row exists. The producer schema coupling is runtime-only - the curated 22023 names the missing dependency when ast_graph.repository_registry is absent, and 22023 also covers an unknown repository id or a status outside (active, paused).';

CREATE OR REPLACE FUNCTION pgokf.registry_set_poll_interval(repository_id uuid, poll_interval_seconds integer)
RETURNS void
LANGUAGE plpgsql
VOLATILE
SECURITY DEFINER
SET search_path = pg_catalog, pg_temp
AS $registry_set_poll_interval$
BEGIN
    IF pg_catalog.to_regclass('ast_graph.repository_registry') IS NULL THEN
        RAISE EXCEPTION
            'registry writes require the producer service schema (ast_graph.repository_registry), which is not present in this database'
            USING ERRCODE = 'invalid_parameter_value';
    END IF;
    -- The producer polls a repository at most this often (its own floor is
    -- 5 seconds); the day-long ceiling keeps a typo from silencing a
    -- repository for weeks.
    IF poll_interval_seconds < 5 OR poll_interval_seconds > 86400 THEN
        RAISE EXCEPTION
            'poll interval must be between 5 and 86400 seconds, not %', poll_interval_seconds
            USING ERRCODE = 'invalid_parameter_value';
    END IF;
    UPDATE ast_graph.repository_registry AS r
       SET poll_interval_seconds = registry_set_poll_interval.poll_interval_seconds,
           updated_at = pg_catalog.now()
     WHERE r.repository_id = registry_set_poll_interval.repository_id
       AND r.tenant_id = COALESCE(NULLIF(pg_catalog.btrim(pg_catalog.current_setting('pgokf.tenant', true)), ''), 'default');
    IF NOT FOUND THEN
        RAISE EXCEPTION
            'no registered repository with id %', repository_id
            USING ERRCODE = 'invalid_parameter_value';
    END IF;
END
$registry_set_poll_interval$;
REVOKE ALL ON FUNCTION pgokf.registry_set_poll_interval(uuid, integer) FROM PUBLIC;
GRANT EXECUTE ON FUNCTION pgokf.registry_set_poll_interval(uuid, integer) TO pgokf_admin;
COMMENT ON FUNCTION pgokf.registry_set_poll_interval(uuid, integer) IS
    'Set one registered repository''s poll interval in seconds (5 to 86400) on the external repository-registry producer service''s ast_graph.repository_registry row: the producer reconciles an active repository at most this often. Admin-only (pgokf_admin), SECURITY DEFINER over a table no API role may write directly; tenant-confined: the update matches only rows whose tenant_id equals the session''s pgokf.tenant setting, with an unset, empty (the GUC''s registered default), or all-whitespace value normalizing to the producer''s ''default'' tenant, so a cross-tenant id earns the same 22023 as an unknown one without revealing that the row exists. The producer schema coupling is runtime-only - the curated 22023 names the missing dependency when ast_graph.repository_registry is absent, and 22023 also covers an unknown repository id or an out-of-range interval.';

-- Last, so any relation a script adds is registered for pg_dump (none here;
-- the rule holds for every upgrade script).
SELECT pgokf_private.register_dump_relations();
