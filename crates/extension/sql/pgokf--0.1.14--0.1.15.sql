-- pgokf extension upgrade: 0.1.14 -> 0.1.15
--
-- 0.1.15 makes the BM25 search backend provider-selectable. The `bm25`
-- search_backend now runs on either Tiger Data pg_textsearch (PostgreSQL
-- license; PostgreSQL 17 and 18) or ParadeDB pg_search, chosen by the new
-- durable policy key `bm25_provider` (auto | pg_textsearch | pg_search; the
-- default `auto` prefers pg_textsearch when it is installed). Both providers
-- register an index access method named bm25 and cannot coexist in one
-- database, so the resolution is unambiguous.
--
-- This script adds the policy column with its default and check constraint,
-- refreshes the object comments whose text changed, and relabels the three
-- search entry points PARALLEL RESTRICTED. Every function that consults the
-- key lives in the shared library; loading the 0.1.15 library activates
-- provider selection, the pg_textsearch query path, the provider-aware index
-- build, and the provider fields of search_index_status(). No existing row is
-- touched, and a catalog upgraded with this script is identical to a fresh
-- 0.1.15 install (the column is the last one in both).
--
-- Never DROP, TRUNCATE, DELETE, or rewrite existing catalog data in an upgrade
-- script: doing so would break the no-data-loss guarantee asserted by the
-- api_stability upgrade tests.

-- IF NOT EXISTS and the duplicate_object guard keep each step idempotent, as
-- in the earlier upgrade scripts.
ALTER TABLE pgokf_private.config
    ADD COLUMN IF NOT EXISTS bm25_provider text NOT NULL DEFAULT 'auto';

DO $pgokf_bm25_provider_chk$
BEGIN
    ALTER TABLE pgokf_private.config
        ADD CONSTRAINT config_bm25_provider_chk
        CHECK (bm25_provider IN ('auto', 'pg_search', 'pg_textsearch'));
EXCEPTION WHEN duplicate_object THEN
    NULL;
END
$pgokf_bm25_provider_chk$;

COMMENT ON COLUMN pgokf_private.config.bm25_provider IS
    'Which BM25 provider extension the bm25 search backend uses: ''auto'' (the default: pg_textsearch when installed, else pg_search), ''pg_textsearch'' (Tiger Data, PostgreSQL license, PostgreSQL 17 and 18), or ''pg_search'' (ParadeDB, AGPL-3.0). Both providers name their index access method bm25 and cannot coexist in one database. A named provider that is not installed makes bm25 search fall back to native with a warning; rebuild_search_index builds the resolved provider''s index.';

-- Comments whose text changed with provider selection, so an upgraded catalog
-- reads identically to a fresh one (COMMENT ON is metadata only).
COMMENT ON COLUMN pgokf_private.config.search_backend IS
    'Ranked-search execution backend for pgokf.concept_search: ''native'' (the default, zero-dependency PostgreSQL FTS available on every supported server) or ''bm25'' (BM25 top-k over the external provider extension that bm25_provider resolves to: Tiger Data pg_textsearch or ParadeDB pg_search). When set to ''bm25'' the search transparently falls back to native, with a warning, if the resolved provider is not installed or no bm25 index exists on pgokf.concepts (build one with pgokf.rebuild_search_index).';

COMMENT ON FUNCTION pgokf.bm25_hits(text, bigint, bigint, text, text, text[], text, text, real, bigint, text) IS
    'Internal helper behind concept_search when search_backend = bm25 resolves to the ParadeDB pg_search provider (the pg_textsearch provider runs inline with invoker rights and does not use it); not part of the stable API. Runs the ParadeDB pg_search BM25 hit query with the owner''s privileges (row-level security wraps the catalog tables in a shape pg_search cannot plan for non-owners) while applying the same pgokf.tenant scoping the policies enforce, over active bundles only, with concept_search''s filters, keyset cursor, and limit. Reader-level; returns exactly the rows concept_search would.';

COMMENT ON FUNCTION pgokf.concept_search(text, bigint, integer, text, text[], text, text, jsonb) IS
    'Rank catalog concepts. Reader-level; searches active bundles only (enabled AND not retired). Optional structured filters (each a no-op when NULL): concept_type (exact type), tags (ALL-of containment), status and trust_tier (from concept_provenance). Stable total order rank DESC, bundle_id ASC, concept_id ASC; pass after_cursor (a {rank,bundle_id,concept_id} JSON object copied from the previous page''s last row) for OFFSET-free keyset pagination (a malformed cursor raises 22023). Uses the search_backend configuration: native full-text search (websearch_to_tsquery + ts_rank_cd) by default, or BM25 top-k through the provider the bm25_provider policy resolves to (Tiger Data pg_textsearch or ParadeDB pg_search) when set to bm25, falling back to native if the provider or its index is absent.';

COMMENT ON FUNCTION pgokf.rebuild_search_index() IS
    'Admin-only. (Re)build the bm25 index on pgokf.concepts used by search_backend=bm25, with the provider the bm25_provider policy resolves to (pg_textsearch, or ParadeDB pg_search); returns true when built, or false (with a NOTICE) when no provider is installed.';

COMMENT ON FUNCTION pgokf.search_index_status() IS
    'Search-index health and coverage (jsonb) for operators: search_backend (configured), native (always true), bm25 {available (a usable provider is installed), provider (pg_textsearch or pg_search as resolved from the bm25_provider setting, else null), provider_setting, index_exists, indexed_rows, total_rows, coverage_pct} and embedding {pgvector_available, index_exists, embedded_rows, total_concepts, coverage_pct, dim}. Reader-level, STABLE, invoker rights; coverage counts are tenant-scoped (RLS-filtered). bm25 coverage is all-or-nothing (the index spans every concept row); embedding coverage is the fraction of concepts with a stored vector.';

-- The functions that dispatch a ranked search through the backend become
-- PARALLEL RESTRICTED (leader only): the bm25 provider's scoring is declared
-- PARALLEL UNSAFE upstream and runs through SPI inside them. Results and
-- signatures are unchanged; a parallel outer plan simply keeps these calls in
-- the leader. Matches the fresh 0.1.15 definitions.
ALTER FUNCTION pgokf.concept_search(text, bigint, integer, text, text[], text, text, jsonb) PARALLEL RESTRICTED;
ALTER FUNCTION pgokf.find_similar(text, bigint, integer) PARALLEL RESTRICTED;
ALTER FUNCTION pgokf.concept_search_hybrid(text, real[], bigint, integer) PARALLEL RESTRICTED;

-- Last, so any relation a script adds is registered for pg_dump (none here;
-- the rule holds for every upgrade script).
SELECT pgokf_private.register_dump_relations();
