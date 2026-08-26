-- Full-text search examples for pgokf.
-- concept_search() is the stable API; the required backend is native PostgreSQL FTS.

-- Ranked search with a deterministic caller-visible order.
SELECT id, title, type, rank
FROM pgokf.concept_search('postgresql replication failover')
ORDER BY rank DESC, id ASC
LIMIT 20;

-- Filter the ranked result set using ordinary SQL.
SELECT id, title, tags, rank
FROM pgokf.concept_search('incident response')
WHERE type = 'Playbook'
  AND tags @> ARRAY['oncall']::text[]
ORDER BY rank DESC, id ASC
LIMIT 10;

-- Search Unicode text. Tokenization depends on the configured text-search
-- configuration; exact metadata filters remain available for every language.
SELECT id, title, rank
FROM pgokf.concept_search('数据库 故障排除')
ORDER BY rank DESC, id ASC;

-- Inspect a concept after discovery without depending on backend-specific operators.
WITH matches AS (
  SELECT id, rank
  FROM pgokf.concept_search('gross margin')
  ORDER BY rank DESC, id ASC
  LIMIT 5
)
SELECT c.id, c.title, c.description, c.resource, c.tags, m.rank
FROM matches AS m
JOIN pgokf.concepts AS c USING (id)
ORDER BY m.rank DESC, c.id ASC;
