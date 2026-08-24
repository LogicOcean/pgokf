-- pgokf quickstart
-- psql variables make the bundle path reusable:
--   psql -v bundle_path=/absolute/path/to/tests/bundles/minimal-valid -f quickstart.sql

CREATE EXTENSION IF NOT EXISTS pgokf;

-- The path is read by the PostgreSQL server, not by the psql client.
SELECT *
FROM pgokf.register_bundle(:'bundle_path');

-- Browse the catalog projection.
SELECT id, title, type, tags
FROM pgokf.concepts
ORDER BY id;

-- Basic full-text search through the backend-independent API.
SELECT *
FROM pgokf.concept_search('replication failover')
LIMIT 10;
