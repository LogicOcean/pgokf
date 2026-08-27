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
    IF NOT EXISTS (SELECT 1 FROM pg_catalog.pg_roles WHERE rolname = 'pgokf_admin') THEN
        CREATE ROLE pgokf_admin NOLOGIN;
    END IF;
END
$pgokf$;

REVOKE ALL ON SCHEMA pgokf FROM PUBLIC;
REVOKE ALL ON SCHEMA pgokf_private FROM PUBLIC;

-- Readers may only see the public API schema; the private schema is reserved
-- for internal catalog state reachable by administrators alone.
GRANT USAGE ON SCHEMA pgokf TO pgokf_reader;
GRANT USAGE ON SCHEMA pgokf TO pgokf_admin;
GRANT USAGE ON SCHEMA pgokf_private TO pgokf_admin;

-- Administrators can use reader-facing search APIs without a separate grant.
GRANT pgokf_reader TO pgokf_admin;

-- Document the two public API roles. Roles are cluster-wide shared objects,
-- so these comments live in pg_shdescription and persist independently of the
-- extension; setting them here keeps the whole public surface self-describing.
COMMENT ON ROLE pgokf_reader IS
    'pgokf read-only API role: may search the catalog and read configuration (concept_search, concept_neighbors, list_bundles, bundle_info, get_config).';
COMMENT ON ROLE pgokf_admin IS
    'pgokf administrative API role: may register, refresh, and unregister bundles and manage configuration; inherits pgokf_reader.';
