-- Runs once, on first cluster initialization, via the postgres image's
-- /docker-entrypoint-initdb.d hook. Makes `pgokf` available in the default
-- database so the image is usable with zero extra steps, and creates the
-- optional extensions the image was built with when the server is configured
-- to load them.
--
-- Registering an OKF bundle still requires a server-readable absolute bundle
-- path and membership in pgokf_writer; see the project README.
CREATE EXTENSION IF NOT EXISTS pgokf;

DO $init$
DECLARE
    preload text := current_setting('shared_preload_libraries', true);
BEGIN
    -- pgvector needs no preload: create it whenever the image carries it so
    -- concept_search_semantic / concept_search_hybrid work immediately.
    IF EXISTS (SELECT 1 FROM pg_available_extensions WHERE name = 'vector') THEN
        CREATE EXTENSION IF NOT EXISTS vector;
    END IF;

    -- pg_search must be in shared_preload_libraries before it is created (the
    -- documented ParadeDB requirement); only auto-create it when the operator
    -- has preloaded it. It pulls in `vector` via CASCADE.
    IF EXISTS (SELECT 1 FROM pg_available_extensions WHERE name = 'pg_search')
       AND preload ~ '(^|,)\s*pg_search\s*(,|$)' THEN
        CREATE EXTENSION IF NOT EXISTS pg_search CASCADE;
    END IF;

    -- pg_cron may only be created in the one database named by
    -- cron.database_name (default: postgres), and only when preloaded.
    IF EXISTS (SELECT 1 FROM pg_available_extensions WHERE name = 'pg_cron')
       AND preload ~ '(^|,)\s*pg_cron\s*(,|$)' THEN
        IF current_database() = coalesce(current_setting('cron.database_name', true), 'postgres') THEN
            CREATE EXTENSION IF NOT EXISTS pg_cron;
        ELSE
            RAISE NOTICE 'pgokf initdb: pg_cron not created in % because cron.database_name is %; set cron.database_name to this database to use schedule_refresh here',
                current_database(), coalesce(current_setting('cron.database_name', true), 'postgres');
        END IF;
    END IF;
END
$init$;

SELECT extname, extversion FROM pg_extension WHERE extname <> 'plpgsql' ORDER BY extname;
