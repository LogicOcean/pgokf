// SPDX-License-Identifier: AGPL-3.0-only
//! The external repository-registry surface: how the catalog's roles reach
//! the producer service's `ast_graph.repository_registry` table when that
//! service shares this database.
//!
//! A registered repository's row (key, project, branch, remote, status, poll
//! interval, and publication evidence) is owned by the repository-registry
//! producer service, a separate codebase whose migrations create the
//! `ast_graph` schema. The coupling is **runtime-only**, exactly like the
//! `pg_cron` adapter in [`crate::catalog::schedule`]: the extension is
//! compiled and installed with no build-time reference to that schema, the
//! reader grant below applies only when the table is present at install or
//! upgrade time, and the write functions resolve it at call time, raising a
//! clear SQLSTATE `22023` naming the missing dependency rather than failing
//! on an absent relation or pretending to succeed.
//!
//! # Grants
//!
//! Reads follow the narrow grant pattern: `pgokf_reader` gets `USAGE` on the
//! `ast_graph` schema and `SELECT` on exactly the columns the admin UI lists -
//! never `checkout_path` (a server-local path) or `graph_id` (the producer's
//! internal key), and there is no secret column to begin with: fetch
//! credentials live behind the producer's admin API, which never returns
//! them. `tenant_id` is granted so callers can confine their read to the
//! session tenant (`pgokf.tenant`, the same GUC the catalog's row-level
//! security enforces); the table itself stays without RLS, so the predicate
//! is the boundary. Writes (`pause`/`resume` as a status change, and the poll
//! interval) go through the `SECURITY DEFINER` functions below, granted to
//! `pgokf_admin`; no API role holds a direct write grant on the table. Both
//! writers confine their update to the session tenant the same way, so a
//! cross-tenant id is the same "no registered repository" 22023 an unknown
//! one earns - the answer never reveals that another tenant's row exists.

use pgrx::extension_sql;

extension_sql!(
    r"
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

CREATE FUNCTION pgokf.registry_set_status(repository_id uuid, status text)
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
       -- never reveals that another tenant's repository exists.
       AND r.tenant_id = COALESCE(pg_catalog.current_setting('pgokf.tenant', true), 'default');
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
    'Pause or resume one registered repository of the external repository-registry producer service by setting its ast_graph.repository_registry status (active or paused; the producer polls active rows only, so pausing stops reconciliation without deleting the registration). Admin-only (pgokf_admin), SECURITY DEFINER over a table no API role may write directly; tenant-confined: the update matches only rows whose tenant_id equals the session''s pgokf.tenant setting (default ''default''), so a cross-tenant id earns the same 22023 as an unknown one without revealing that the row exists. The producer schema coupling is runtime-only - the curated 22023 names the missing dependency when ast_graph.repository_registry is absent, and 22023 also covers an unknown repository id or a status outside (active, paused).';

CREATE FUNCTION pgokf.registry_set_poll_interval(repository_id uuid, poll_interval_seconds integer)
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
       AND r.tenant_id = COALESCE(pg_catalog.current_setting('pgokf.tenant', true), 'default');
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
    'Set one registered repository''s poll interval in seconds (5 to 86400) on the external repository-registry producer service''s ast_graph.repository_registry row: the producer reconciles an active repository at most this often. Admin-only (pgokf_admin), SECURITY DEFINER over a table no API role may write directly; tenant-confined: the update matches only rows whose tenant_id equals the session''s pgokf.tenant setting (default ''default''), so a cross-tenant id earns the same 22023 as an unknown one without revealing that the row exists. The producer schema coupling is runtime-only - the curated 22023 names the missing dependency when ast_graph.repository_registry is absent, and 22023 also covers an unknown repository id or an out-of-range interval.';
",
    name = "registry_surface",
    requires = ["catalog_tables"]
);
