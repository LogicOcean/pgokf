-- OKF v0.2 link-graph examples.
-- pgokf.links stores directed Markdown links extracted during synchronization.

-- Direct outgoing links from one concept.
SELECT source_id, target_id, link_text, target_path, resolved, is_external
FROM pgokf.links
WHERE source_id = 'runbooks/database-failover'
ORDER BY target_id NULLS LAST, target_path;

-- Backlinks: concepts that link to the selected concept.
SELECT source_id, link_text
FROM pgokf.links
WHERE target_id = 'services/postgresql'
  AND resolved
ORDER BY source_id;

-- Cycle-safe traversal within one bundle.
-- Prefer the built-in pgokf.concept_neighbors(concept_id, max_hops, bundle_id),
-- which caps depth at the pgokf.max_graph_hops GUC and excludes external and
-- unresolved edges; the recursive CTE below shows the equivalent hand-written form.
SELECT source_id, neighbor_id, hops, path, title
FROM pgokf.concept_neighbors(:'start_concept', 8, :'bundle_id')
ORDER BY hops, neighbor_id;

WITH RECURSIVE walk AS (
  SELECT
    l.bundle_id,
    l.source_id,
    l.target_id,
    1 AS depth,
    ARRAY[l.source_id, l.target_id]::text[] AS path
  FROM pgokf.links AS l
  WHERE l.bundle_id = :'bundle_id'::bigint
    AND l.source_id = :'start_concept'
    AND l.resolved
    AND NOT l.is_external

  UNION ALL

  SELECT
    l.bundle_id,
    l.source_id,
    l.target_id,
    w.depth + 1,
    w.path || l.target_id
  FROM walk AS w
  JOIN pgokf.links AS l
    ON l.bundle_id = w.bundle_id
   AND l.source_id = w.target_id
  WHERE l.resolved
    AND NOT l.is_external
    AND w.depth < 8
    AND NOT l.target_id = ANY (w.path)
)
SELECT source_id, target_id, depth, path
FROM walk
ORDER BY depth, source_id, target_id;

-- Unresolved internal links are retained for graph-quality checks.
SELECT source_id, target_path, link_text
FROM pgokf.links
WHERE NOT resolved
  AND NOT is_external
ORDER BY source_id, target_path;
