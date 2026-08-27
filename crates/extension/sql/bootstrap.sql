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
