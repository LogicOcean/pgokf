-- pgokf extension bootstrap objects.
-- PostgreSQL roles are cluster-wide and cannot be extension members, so the
-- idempotent blocks below create them only when they do not already exist.

CREATE SCHEMA IF NOT EXISTS pgokf;
CREATE SCHEMA IF NOT EXISTS pgokf_private;

DO $pgokf$
BEGIN
    IF NOT EXISTS (SELECT 1 FROM pg_catalog.pg_roles WHERE rolname = 'pgokf_reader') THEN
        CREATE ROLE pgokf_reader NOLOGIN;
    END IF;
    IF NOT EXISTS (SELECT 1 FROM pg_catalog.pg_roles WHERE rolname = 'pgokf_writer') THEN
        CREATE ROLE pgokf_writer NOLOGIN;
    END IF;
    IF NOT EXISTS (SELECT 1 FROM pg_catalog.pg_roles WHERE rolname = 'pgokf_admin') THEN
        CREATE ROLE pgokf_admin NOLOGIN;
    END IF;
END
$pgokf$;

REVOKE ALL ON SCHEMA pgokf FROM PUBLIC;
REVOKE ALL ON SCHEMA pgokf_private FROM PUBLIC;

-- Every tier needs to reach the public API schema to run its functions; the
-- private schema stays reserved for internal catalog state that only
-- administrators may reach.
GRANT USAGE ON SCHEMA pgokf TO pgokf_reader;
GRANT USAGE ON SCHEMA pgokf TO pgokf_writer;
GRANT USAGE ON SCHEMA pgokf TO pgokf_admin;
GRANT USAGE ON SCHEMA pgokf_private TO pgokf_admin;

-- Least-privilege role hierarchy: reader < writer < admin. Granting the lower
-- role to the higher one makes each tier inherit everything below it, so a
-- writer can also search and an admin can also ingest and search. pg_has_role
-- resolves this chain, so the in-function membership checks accept a higher
-- tier wherever a lower one is required.
GRANT pgokf_reader TO pgokf_writer;
GRANT pgokf_writer TO pgokf_admin;

-- Document the three public API roles. Roles are cluster-wide shared objects,
-- so these comments live in pg_shdescription and persist independently of the
-- extension; setting them here keeps the whole public surface self-describing.
COMMENT ON ROLE pgokf_reader IS
    'pgokf read-only API role: may search the catalog and read configuration (concept_search, concept_neighbors, list_bundles, bundle_info, get_config, get_concept_source).';
COMMENT ON ROLE pgokf_writer IS
    'pgokf ingestion API role: may register, refresh, and unregister bundles; inherits pgokf_reader. Intended account for an automated ingestion pipeline / the content-ingestion API. Does not include configuration or file-writing exports.';
COMMENT ON ROLE pgokf_admin IS
    'pgokf administrative API role: everything a writer can do plus configuration (set_config/reset_config) and file-writing exports (export_parquet/export_sources); inherits pgokf_writer (and thus pgokf_reader).';

-- Register every extension-owned table and sequence with pg_dump, so a logical
-- backup carries the catalog's rows and sequence positions rather than just the
-- CREATE EXTENSION statement. Discovered from the extension's own dependency
-- graph, so nothing is listed by hand. pg_extension_config_dump may only run
-- from an extension script: the fresh install calls this at the end of its
-- SQL, and EVERY upgrade script that creates a table or sequence must call it
-- again as its last statement.
CREATE FUNCTION pgokf_private.register_dump_relations() RETURNS void
    LANGUAGE plpgsql
    SET search_path = pg_catalog, pg_temp
    AS $fn$
DECLARE
    rel regclass;
BEGIN
    FOR rel IN
        SELECT c.oid::regclass
          FROM pg_catalog.pg_depend d
          JOIN pg_catalog.pg_extension e ON e.oid = d.refobjid AND e.extname = 'pgokf'
          JOIN pg_catalog.pg_class c ON c.oid = d.objid
         WHERE d.classid = 'pg_catalog.pg_class'::regclass
           AND d.refclassid = 'pg_catalog.pg_extension'::regclass
           AND d.deptype = 'e'
           AND c.relkind IN ('r', 'S')
         ORDER BY c.oid
    LOOP
        PERFORM pg_catalog.pg_extension_config_dump(rel, '');
    END LOOP;
END
$fn$;

REVOKE ALL ON FUNCTION pgokf_private.register_dump_relations() FROM PUBLIC;
COMMENT ON FUNCTION pgokf_private.register_dump_relations() IS
    'Internal, extension-script only: registers every pgokf-owned table and sequence with pg_extension_config_dump so pg_dump carries the catalog. Called at the end of the fresh install and of every upgrade script that adds a relation.';
