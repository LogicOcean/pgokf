-- pgokf extension upgrade: 0.1.8 -> 0.1.9
--
-- This upgrade carries the COMPLETE delta from a fresh 0.1.8 install up to a
-- fresh 0.1.9 install, so that `ALTER EXTENSION pgokf UPDATE TO '0.1.9'` yields a
-- catalog functionally identical to `CREATE EXTENSION pgokf` at 0.1.9. The whole
-- file runs in a single transaction.
--
-- 0.1.9 is one additive search/scheduling feature batch:
--   F1 keyset / cursor pagination on concept_search (a new optional trailing
--      after_cursor jsonb argument; the ranked total order is now stable —
--      rank DESC, bundle_id ASC, concept_id ASC);
--   F2 faceted result counts (pgokf.search_facet composite, pgokf.search_facets);
--   F3 search-index health / coverage (pgokf.search_index_status);
--   F4 optional pg_cron scheduled re-sync (pgokf.schedule_refresh /
--      unschedule_refresh), reached only through runtime SPI.
--
-- Behavior that lives entirely in the 0.1.9 shared library needs no SQL here: the
-- keyset predicate and the stable total order are compiled into the two search
-- backends, the facet dispatch into search_facets, the coverage probes into
-- search_index_status, and the pg_cron optional-SPI path into schedule_refresh /
-- unschedule_refresh. This script only (re)declares the SQL-callable functions
-- and the one new composite type, with the C symbols, signatures, and
-- STRICT / STABLE / PARALLEL SAFE / SECURITY markers the 0.1.9 shared library
-- exports, so an upgraded catalog resolves and authorizes them byte-identically
-- to a fresh install. Every statement is additive and non-destructive with the
-- single, carefully-justified exception in Step 1 (the concept_search overload
-- replacement, exactly as 0.1.5->0.1.6 did): no row is truncated or deleted, and
-- no data-bearing object is dropped.

-- ===========================================================================
-- Step 1: the concept_search keyset-pagination signature change (F1).
--
-- 0.1.9 extends concept_search with one optional trailing argument,
-- after_cursor jsonb, so the fresh schema emits ONE function whose identity is
--   concept_search(text, bigint, integer, text, text[], text, text, jsonb).
-- A trailing-default argument list is a DIFFERENT function identity in pg_proc
-- from the 0.1.8 concept_search(text, bigint, integer, text, text[], text, text);
-- they are two separate rows, not a redefinition, and pgrx regenerates only the
-- new one on a fresh install.
--
-- Therefore, to make an UPGRADED catalog byte-identical to a FRESH 0.1.9 install
-- — the release invariant this whole file upholds — the superseded 0.1.8
-- seven-argument overload MUST be removed here. Leaving it would (a) make pg_proc
-- carry two concept_search overloads where a fresh install carries one, and
-- (b) make a call such as concept_search('q', NULL, 20, NULL, NULL, NULL, NULL)
-- ambiguous between the two, breaking the very backward compatibility this
-- release preserves. This DROP is NOT data-destructive: concept_search is a
-- function, not a table; no row is touched; and it is replaced in the same
-- transaction by a STRICT SUPERSET that resolves every historical three- through
-- seven-argument call through its defaults (after_cursor defaults to NULL = the
-- first page, the pre-0.1.9 behavior). This is the single, deliberate exception
-- to the file's otherwise purely-additive rule, and it is required for
-- correctness — verified by comparing the concept_search overload set of an
-- upgraded vs. a fresh catalog. It mirrors the 0.1.5->0.1.6 overload replacement.
DROP FUNCTION IF EXISTS pgokf.concept_search(text, bigint, integer, text, text[], text, text);

CREATE FUNCTION pgokf."concept_search"(
    "query" text,
    "bundle_id" bigint DEFAULT NULL,
    "limit_count" integer DEFAULT 20,
    "concept_type" text DEFAULT NULL,
    "tags" text[] DEFAULT NULL,
    "status" text DEFAULT NULL,
    "trust_tier" text DEFAULT NULL,
    "after_cursor" jsonb DEFAULT NULL
) RETURNS SETOF pgokf.concept_search_result
STABLE PARALLEL SAFE
LANGUAGE c
AS 'MODULE_PATHNAME', 'concept_search_wrapper';

REVOKE ALL ON FUNCTION pgokf.concept_search(text, bigint, integer, text, text[], text, text, jsonb) FROM PUBLIC;
GRANT EXECUTE ON FUNCTION pgokf.concept_search(text, bigint, integer, text, text[], text, text, jsonb) TO pgokf_reader;
COMMENT ON FUNCTION pgokf.concept_search(text, bigint, integer, text, text[], text, text, jsonb) IS
    'Rank catalog concepts. Reader-level; searches active bundles only (enabled AND not retired). Optional structured filters (each a no-op when NULL): concept_type (exact type), tags (ALL-of containment), status and trust_tier (from concept_provenance). Stable total order rank DESC, bundle_id ASC, concept_id ASC; pass after_cursor (a {rank,bundle_id,concept_id} JSON object copied from the previous page''s last row) for OFFSET-free keyset pagination (a malformed cursor raises 22023). Uses the search_backend configuration: native full-text search (websearch_to_tsquery + ts_rank_cd) by default, or ParadeDB pg_search BM25 when set to bm25 (falling back to native if pg_search or its index is absent).';

-- ===========================================================================
-- Step 2: faceted result counts (F2).
--
-- The pgokf.search_facet composite and the reader-level pgokf.search_facets
-- projection. Invoker rights (RLS-filtered), so no SECURITY DEFINER. Matches the
-- fresh 0.1.9 search_facet_type / search_facets blocks statement for statement.
-- ===========================================================================
CREATE TYPE pgokf.search_facet AS (
    facet_value text,
    count       bigint
);

COMMENT ON TYPE pgokf.search_facet IS
    'One faceted-count bucket from pgokf.search_facets: a distinct facet value (a type, bundle id, status, trust tier, or tag) and how many matching concepts carry it.';

CREATE FUNCTION pgokf."search_facets"(
    "query" text,
    "bundle_id" bigint DEFAULT NULL,
    "facet" text DEFAULT 'type',
    "concept_type" text DEFAULT NULL,
    "tags" text[] DEFAULT NULL,
    "status" text DEFAULT NULL,
    "trust_tier" text DEFAULT NULL
) RETURNS SETOF pgokf.search_facet
STABLE PARALLEL SAFE
LANGUAGE c
AS 'MODULE_PATHNAME', 'search_facets_wrapper';

REVOKE ALL ON FUNCTION pgokf.search_facets(text, bigint, text, text, text[], text, text) FROM PUBLIC;
GRANT EXECUTE ON FUNCTION pgokf.search_facets(text, bigint, text, text, text[], text, text) TO pgokf_reader;
COMMENT ON FUNCTION pgokf.search_facets(text, bigint, text, text, text[], text, text) IS
    'Count the concept_search matching set (native FTS match of query plus the same optional concept_type/tags/status/trust_tier filters) grouped by facet, as pgokf.search_facet. facet is one of type, bundle, status, trust_tier, tag (else 22023); the facet is dispatched on, never interpolated. Reader-level, STABLE, invoker rights (RLS-filtered to the tenant); active bundles only. The tag facet counts a concept once per tag; NULL facet values are omitted; ordered by count DESC then value.';

-- ===========================================================================
-- Step 3: search-index health / coverage (F3).
--
-- Reader-level pgokf.search_index_status(): a jsonb document reporting the
-- configured backend and, for each optional index, availability, existence, and
-- coverage. Invoker rights (RLS-filtered coverage counts). Matches the fresh
-- 0.1.9 search_index_status block.
-- ===========================================================================
CREATE FUNCTION pgokf."search_index_status"() RETURNS jsonb
STRICT STABLE
LANGUAGE c
AS 'MODULE_PATHNAME', 'search_index_status_wrapper';

REVOKE ALL ON FUNCTION pgokf.search_index_status() FROM PUBLIC;
GRANT EXECUTE ON FUNCTION pgokf.search_index_status() TO pgokf_reader;
COMMENT ON FUNCTION pgokf.search_index_status() IS
    'Search-index health and coverage (jsonb) for operators: search_backend (configured), native (always true), bm25 {available (pg_search installed), index_exists, indexed_rows, total_rows, coverage_pct} and embedding {pgvector_available, index_exists, embedded_rows, total_concepts, coverage_pct, dim}. Reader-level, STABLE, invoker rights; coverage counts are tenant-scoped (RLS-filtered). bm25 coverage is all-or-nothing (the index spans every concept row); embedding coverage is the fraction of concepts with a stored vector.';

-- ===========================================================================
-- Step 4: optional pg_cron scheduled re-sync (F4).
--
-- Admin-tier, SECURITY DEFINER schedule / unschedule toggles reached only through
-- runtime SPI: CREATE EXTENSION pgokf and this upgrade both succeed where pg_cron
-- is absent, and every cron.* object is touched only at call time. schedule_refresh
-- raises 22023 naming the missing pg_cron dependency when it is not installed
-- (mirroring concept_search_semantic for pgvector); unschedule_refresh is then a
-- clean no-op. Matches the fresh 0.1.9 schedule_refresh / unschedule_refresh
-- blocks. The wrapper symbols are exported by the 0.1.9 shared library.
-- ===========================================================================
CREATE FUNCTION pgokf."schedule_refresh"(
    "bundle_id" bigint,
    "schedule" text
) RETURNS text
STRICT
LANGUAGE c
AS 'MODULE_PATHNAME', 'schedule_refresh_wrapper';

CREATE FUNCTION pgokf."unschedule_refresh"(
    "bundle_id" bigint
) RETURNS bool
STRICT
LANGUAGE c
AS 'MODULE_PATHNAME', 'unschedule_refresh_wrapper';

ALTER FUNCTION pgokf.schedule_refresh(bigint, text)
    SECURITY DEFINER SET search_path = pg_catalog, pg_temp;
ALTER FUNCTION pgokf.unschedule_refresh(bigint)
    SECURITY DEFINER SET search_path = pg_catalog, pg_temp;
REVOKE ALL ON FUNCTION pgokf.schedule_refresh(bigint, text) FROM PUBLIC;
REVOKE ALL ON FUNCTION pgokf.unschedule_refresh(bigint) FROM PUBLIC;
GRANT EXECUTE ON FUNCTION pgokf.schedule_refresh(bigint, text) TO pgokf_admin;
GRANT EXECUTE ON FUNCTION pgokf.unschedule_refresh(bigint) TO pgokf_admin;
COMMENT ON FUNCTION pgokf.schedule_refresh(bigint, text) IS
    'Schedule a recurring pgokf.refresh_bundle(<bundle_id>) via pg_cron under the deterministic job name pgokf_refresh_<bundle_id> (idempotent/re-schedulable), returning the job name. Admin-only, SECURITY DEFINER, tenant-confined. The scheduled command is a fixed SELECT pgokf.refresh_bundle(<id>) with the id as a trusted integer literal; the schedule and job name bind as parameters. Requires pg_cron: raises 22023 naming the missing dependency when it is not installed (no silent success), and 22023 for an unknown/cross-tenant bundle_id or an empty/oversized schedule. Full scheduling requires pg_cron in shared_preload_libraries.';
COMMENT ON FUNCTION pgokf.unschedule_refresh(bigint) IS
    'Remove the pgokf_refresh_<bundle_id> pg_cron refresh job when present (returns true); a clean no-op returning false (with a NOTICE) when pg_cron is not installed or no such job exists. Admin-only, SECURITY DEFINER, tenant-confined; raises 22023 for an unknown/cross-tenant bundle_id.';
