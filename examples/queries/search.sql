-- Full-text search examples for pgokf.
--
-- pgokf.concept_search(query, bundle_id, limit_count) is the stable API; the
-- required backend is native PostgreSQL FTS. Its result columns are:
--   bundle_id, concept_id, path, title, type, rank, headline
-- Note the search key is concept_id (not id), and there is no tags column --
-- join pgokf.concepts to recover tags/description. The queries below run
-- against the sample bundle in examples/sample-bundle.

-- Ranked search with the caller-visible deterministic order (rank desc, id asc).
SELECT concept_id, title, type, rank
FROM pgokf.concept_search('postgres failover')
ORDER BY rank DESC, concept_id ASC
LIMIT 20;

-- Filter the ranked result set using ordinary SQL, joining pgokf.concepts to
-- reach columns concept_search does not return (here: tags).
SELECT s.concept_id, s.title, c.tags, s.rank
FROM pgokf.concept_search('incident response') AS s
JOIN pgokf.concepts AS c
  ON c.bundle_id = s.bundle_id AND c.id = s.concept_id
WHERE s.type = 'Runbook'
  AND c.tags @> ARRAY['oncall']::text[]
ORDER BY s.rank DESC, s.concept_id ASC
LIMIT 10;

-- Read the ts_headline snippet that accompanies each hit.
SELECT concept_id, headline
FROM pgokf.concept_search('failover standby')
ORDER BY rank DESC, concept_id ASC
LIMIT 5;

-- Scope a search to a single bundle by passing bundle_id (the second argument).
-- Replace 1 with an id from pgokf.list_bundles().
SELECT concept_id, title, rank
FROM pgokf.concept_search('replication', 1)
ORDER BY rank DESC, concept_id ASC;

-- Search Unicode text. Tokenization depends on the configured text-search
-- configuration; exact metadata filters remain available for every language.
SELECT concept_id, title, rank
FROM pgokf.concept_search('数据库 故障排除')
ORDER BY rank DESC, concept_id ASC;

-- Inspect matched concepts in full by joining back to pgokf.concepts.
WITH matches AS (
  SELECT bundle_id, concept_id, rank
  FROM pgokf.concept_search('service health')
  ORDER BY rank DESC, concept_id ASC
  LIMIT 5
)
SELECT c.id, c.title, c.description, c.resource, c.tags, m.rank
FROM matches AS m
JOIN pgokf.concepts AS c
  ON c.bundle_id = m.bundle_id AND c.id = m.concept_id
ORDER BY m.rank DESC, c.id ASC;
