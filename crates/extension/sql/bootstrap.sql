CREATE SCHEMA IF NOT EXISTS pgokf;

DO $roles$
BEGIN
    IF NOT EXISTS (SELECT 1 FROM pg_catalog.pg_roles WHERE rolname = 'pgokf_reader') THEN
        CREATE ROLE pgokf_reader NOLOGIN;
    END IF;
    IF NOT EXISTS (SELECT 1 FROM pg_catalog.pg_roles WHERE rolname = 'pgokf_admin') THEN
        CREATE ROLE pgokf_admin NOLOGIN;
    END IF;
END
$roles$;

GRANT USAGE ON SCHEMA pgokf TO pgokf_reader, pgokf_admin;
GRANT pgokf_reader TO pgokf_admin;
