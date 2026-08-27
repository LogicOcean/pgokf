-- pgokf quickstart
--
-- Run against the sample bundle shipped in examples/sample-bundle, passing its
-- ABSOLUTE path as a psql variable (the path is read by the PostgreSQL server,
-- not by the psql client):
--
--   psql -v bundle_path=/abs/path/to/examples/sample-bundle -f quickstart.sql
--
-- Registration requires membership in pgokf_admin.

CREATE EXTENSION IF NOT EXISTS pgokf;

-- Confirm the loaded module version.
SELECT pgokf.version();

-- Register and synchronize the bundle. Reserved files (index.md, log.md) are
-- skipped, so the sample bundle reports added = 4.
SELECT *
FROM pgokf.register_bundle(:'bundle_path');

-- Browse the catalog projection.
SELECT id, title, type, tags
FROM pgokf.concepts
ORDER BY id;

-- Ranked full-text search through the backend-independent API.
SELECT concept_id, title, type
FROM pgokf.concept_search('postgres failover')
LIMIT 10;

-- Walk the resolved internal link graph outward from one concept.
SELECT source_id, neighbor_id, hops, path
FROM pgokf.concept_neighbors('runbooks/database-failover', 2)
ORDER BY hops, neighbor_id;

-- Inspect the provenance projection (only concepts with provenance frontmatter).
SELECT concept_id, generated_by, verified, verification_method, freshness
FROM pgokf.concept_provenance
ORDER BY concept_id;
