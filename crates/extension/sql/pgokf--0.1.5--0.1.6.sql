-- pgokf extension upgrade: 0.1.5 -> 0.1.6
--
-- This upgrade carries the COMPLETE delta from a fresh 0.1.5 install up to a
-- fresh 0.1.6 install, so that `ALTER EXTENSION pgokf UPDATE TO '0.1.6'` yields a
-- catalog functionally identical to `CREATE EXTENSION pgokf` at 0.1.6. It is one
-- additive search-enhancement batch — structured filters on concept_search, a
-- content more-like-this (find_similar), and an optional pgvector semantic /
-- hybrid surface — so every statement is additive and non-destructive with the
-- single, carefully-justified exception in Step 3 (see there): nothing is
-- truncated, no row is deleted, and existing data keeps its values (the new
-- embedding_dim config column backfills the singleton row from its default). The
-- whole file runs in a single transaction.
--
-- Behavior that lives entirely in the 0.1.6 shared library needs no SQL here: the
-- structured-filter clauses and the backend dispatch are compiled into the
-- concept_search / find_similar / semantic / hybrid wrappers. This script only
-- creates the new SQL objects those code paths read and write (the embedding
-- storage table and config column) and (re)declares the SQL-callable functions,
-- with the C symbols, signatures, and STRICT/STABLE/SECURITY markers the 0.1.6
-- shared library exports, so an upgraded catalog resolves and authorizes them
-- byte-identically to a fresh install.

-- ===========================================================================
-- Step 1: new durable configuration column (embedding_dim).
--
-- Fresh 0.1.6 ships this column on pgokf_private.config; get_config projects it
-- and the embedding paths read it, so it MUST exist before the 0.1.6 module runs.
-- Added with its NOT NULL default so the existing singleton row backfills
-- non-destructively; the range CHECK is added guarded so re-running is idempotent.
-- ===========================================================================
ALTER TABLE pgokf_private.config
    ADD COLUMN IF NOT EXISTS embedding_dim integer NOT NULL DEFAULT 1536;

DO $pgokf_embedding_dim_chk$
BEGIN
    ALTER TABLE pgokf_private.config
        ADD CONSTRAINT config_embedding_dim_chk
        CHECK (embedding_dim BETWEEN 1 AND 16000);
EXCEPTION WHEN duplicate_object THEN
    NULL;
END
$pgokf_embedding_dim_chk$;

COMMENT ON COLUMN pgokf_private.config.embedding_dim IS
    'Expected dimension (1..=16000) of the caller-computed concept embeddings streamed in via pgokf.set_concept_embedding: the setter rejects any real[] whose length differs, and pgokf.rebuild_embedding_index builds its pgvector HNSW index with this typmod (vector(embedding_dim)). Default 1536. The extension never computes embeddings; a change is not retroactive to already-stored rows and should be followed by re-ingestion and pgokf.rebuild_embedding_index. HNSW indexing applies only up to pgvector''s 2000-dimension index limit; above it semantic search still works via an exact scan.';

-- ===========================================================================
-- Step 2: the optional embedding storage table (pgokf.concept_embedding).
--
-- Matches, attribute for attribute, the CREATE TABLE the fresh 0.1.6 schema
-- emits. The vector is stored as the builtin real[], never a pgvector 'vector'
-- column, so this statement (and the whole update) succeeds on a cluster where
-- pgvector is NOT installed; the real[] is cast to vector(dim) only at query and
-- index time. Rows cascade from pgokf.concepts.
-- ===========================================================================
CREATE TABLE pgokf.concept_embedding (
    bundle_id  bigint      NOT NULL,
    concept_id text        NOT NULL,
    embedding  real[]      NOT NULL,
    dim        integer     NOT NULL,
    model      text,
    updated_at timestamptz NOT NULL DEFAULT now(),
    CONSTRAINT concept_embedding_pkey PRIMARY KEY (bundle_id, concept_id),
    CONSTRAINT concept_embedding_concept_fk
        FOREIGN KEY (bundle_id, concept_id)
        REFERENCES pgokf.concepts (bundle_id, id)
        ON DELETE CASCADE,
    CONSTRAINT concept_embedding_dim_chk CHECK (dim = cardinality(embedding))
);

COMMENT ON TABLE pgokf.concept_embedding IS
    'Opt-in per-concept embedding vectors, streamed in by a companion embedder via pgokf.set_concept_embedding (the extension never computes embeddings or performs network I/O). The vector is stored as the builtin real[] — NOT a pgvector ''vector'' column — so CREATE EXTENSION pgokf succeeds without pgvector installed; it is cast to vector(dim) at query time and in the HNSW index only when pgvector is present. Rows cascade from pgokf.concepts, so removing a concept or unregistering a bundle drops its embedding automatically.';
COMMENT ON COLUMN pgokf.concept_embedding.embedding IS
    'The caller-computed embedding as real[]. Its length must equal the durable embedding_dim configuration key at ingest time (enforced by pgokf.set_concept_embedding); dim records that length redundantly for a size-only read.';
COMMENT ON COLUMN pgokf.concept_embedding.dim IS
    'Length of embedding, constrained equal to cardinality(embedding); the effective dimension of the stored vector.';
COMMENT ON COLUMN pgokf.concept_embedding.model IS
    'Optional identifier of the embedding model/producer that computed the vector, for provenance; NULL when not supplied.';
COMMENT ON COLUMN pgokf.concept_embedding.updated_at IS
    'When this embedding row was last written by pgokf.set_concept_embedding.';

GRANT SELECT ON pgokf.concept_embedding TO pgokf_reader;

-- ===========================================================================
-- Step 3: the concept_search structured-filter signature change.
--
-- 0.1.6 extends concept_search with four optional trailing filters, so the fresh
-- schema emits ONE function whose identity is
--   concept_search(text, bigint, integer, text, text[], text, text).
-- A trailing-default argument list is a DIFFERENT function identity in pg_proc
-- from the 0.1.5 concept_search(text, bigint, integer); they are two separate
-- rows, not a redefinition, and pgrx regenerates only the new one on a fresh
-- install.
--
-- Therefore, to make an UPGRADED catalog byte-identical to a FRESH 0.1.6 install
-- — the release invariant this whole file exists to uphold — the superseded
-- 0.1.5 three-argument overload MUST be removed here. Leaving it would (a) make
-- pg_proc carry two concept_search overloads where a fresh install carries one,
-- and (b) make an unqualified call such as concept_search('q') ambiguous between
-- the two, breaking the very backward compatibility this release preserves. This
-- DROP is NOT data-destructive: concept_search is a function, not a table; no row
-- is touched; and it is replaced in the same transaction by a STRICT SUPERSET
-- that resolves every historical one-, two-, and three-argument call through its
-- defaults. This is the single, deliberate exception to the file's otherwise
-- purely-additive rule, and it is required for correctness — verified by
-- comparing the concept_search overload set of an upgraded vs. a fresh catalog.
DROP FUNCTION IF EXISTS pgokf.concept_search(text, bigint, integer);

CREATE FUNCTION pgokf."concept_search"(
    "query" text,
    "bundle_id" bigint DEFAULT NULL,
    "limit_count" integer DEFAULT 20,
    "concept_type" text DEFAULT NULL,
    "tags" text[] DEFAULT NULL,
    "status" text DEFAULT NULL,
    "trust_tier" text DEFAULT NULL
) RETURNS SETOF pgokf.concept_search_result
STABLE PARALLEL SAFE
LANGUAGE c
AS 'MODULE_PATHNAME', 'concept_search_wrapper';

REVOKE ALL ON FUNCTION pgokf.concept_search(text, bigint, integer, text, text[], text, text) FROM PUBLIC;
GRANT EXECUTE ON FUNCTION pgokf.concept_search(text, bigint, integer, text, text[], text, text) TO pgokf_reader;
COMMENT ON FUNCTION pgokf.concept_search(text, bigint, integer, text, text[], text, text) IS
    'Rank catalog concepts. Reader-level; searches enabled bundles only. Optional trailing filters (each a no-op when NULL): concept_type (exact type), tags (ALL-of containment), status and trust_tier (from concept_provenance). Uses the search_backend configuration: native full-text search (websearch_to_tsquery + ts_rank_cd) by default, or ParadeDB pg_search BM25 when set to bm25 (falling back to native if pg_search or its index is absent).';

-- ===========================================================================
-- Step 4: the five new SQL-callable functions.
--
-- Each C symbol (<fn>_wrapper), argument/return signature, and STRICT / STABLE /
-- PARALLEL SAFE / SECURITY DEFINER marker mirrors exactly what the fresh 0.1.6
-- schema emits from crates/extension/src/catalog/{similar,embedding}.rs, so an
-- upgraded catalog resolves and authorizes them byte-identically to a fresh
-- install. The 0.1.6 shared library exports every one of these symbols.
-- ===========================================================================

-- F1: content more-like-this. Reader-level, invoker rights (reads only
-- reader-granted tables and dispatches through the configured search_backend).
CREATE FUNCTION pgokf."find_similar"(
    "concept_id" text,
    "bundle_id" bigint DEFAULT NULL,
    "limit_count" integer DEFAULT 10
) RETURNS SETOF pgokf.concept_search_result
STABLE PARALLEL SAFE
LANGUAGE c
AS 'MODULE_PATHNAME', 'find_similar_wrapper';

REVOKE ALL ON FUNCTION pgokf.find_similar(text, bigint, integer) FROM PUBLIC;
GRANT EXECUTE ON FUNCTION pgokf.find_similar(text, bigint, integer) TO pgokf_reader;
COMMENT ON FUNCTION pgokf.find_similar(text, bigint, integer) IS
    'Content more-like-this: rank concepts by similarity to a seed concept''s body_tsv salient lexemes through the configured search_backend (native FTS or BM25), excluding the seed. Distinct from concept_neighbors (the authored link graph). Reader-level, invoker rights; searches enabled bundles only. Raises 22023 on limit_count out of 1..=500 or an ambiguous concept_id (pass bundle_id).';

-- F2: embedding ingest. Writer-tier, SECURITY DEFINER (writes the embedding
-- table and reads the admin-only config for embedding_dim).
CREATE FUNCTION pgokf."set_concept_embedding"(
    "bundle_id" bigint,
    "concept_id" text,
    "embedding" real[]
) RETURNS void
STRICT
LANGUAGE c
AS 'MODULE_PATHNAME', 'set_concept_embedding_wrapper';

ALTER FUNCTION pgokf.set_concept_embedding(bigint, text, real[])
    SECURITY DEFINER SET search_path = pg_catalog, pg_temp;
REVOKE ALL ON FUNCTION pgokf.set_concept_embedding(bigint, text, real[]) FROM PUBLIC;
GRANT EXECUTE ON FUNCTION pgokf.set_concept_embedding(bigint, text, real[]) TO pgokf_writer;
COMMENT ON FUNCTION pgokf.set_concept_embedding(bigint, text, real[]) IS
    'Store or replace one concept''s embedding (real[]) streamed in by a companion embedder; the extension never computes embeddings. Writer-tier (pgokf_writer; admin inherits it), SECURITY DEFINER. Validates the concept exists and len(embedding)=embedding_dim (else 22023) and upserts. The vector is stored as real[] so pgokf needs no static pgvector dependency.';

-- F3: semantic (vector) search. Reader-level, invoker rights.
CREATE FUNCTION pgokf."concept_search_semantic"(
    "query_embedding" real[],
    "bundle_id" bigint DEFAULT NULL,
    "limit_count" integer DEFAULT 10
) RETURNS SETOF pgokf.concept_search_result
STABLE PARALLEL SAFE
LANGUAGE c
AS 'MODULE_PATHNAME', 'concept_search_semantic_wrapper';

REVOKE ALL ON FUNCTION pgokf.concept_search_semantic(real[], bigint, integer) FROM PUBLIC;
GRANT EXECUTE ON FUNCTION pgokf.concept_search_semantic(real[], bigint, integer) TO pgokf_reader;
COMMENT ON FUNCTION pgokf.concept_search_semantic(real[], bigint, integer) IS
    'Semantic nearest-neighbor search: rank concepts by pgvector cosine distance to query_embedding (rank = normalized cosine similarity). Reader-level, invoker rights; enabled bundles only. query_embedding must have embedding_dim dimensions; limit_count in 1..=500. Requires pgvector: raises 22023 naming the missing dependency when it is not installed (no lexical fallback).';

-- F4: hybrid (RRF) search. Reader-level, invoker rights.
CREATE FUNCTION pgokf."concept_search_hybrid"(
    "query" text,
    "query_embedding" real[],
    "bundle_id" bigint DEFAULT NULL,
    "limit_count" integer DEFAULT 10
) RETURNS SETOF pgokf.concept_search_result
STABLE PARALLEL SAFE
LANGUAGE c
AS 'MODULE_PATHNAME', 'concept_search_hybrid_wrapper';

REVOKE ALL ON FUNCTION pgokf.concept_search_hybrid(text, real[], bigint, integer) FROM PUBLIC;
GRANT EXECUTE ON FUNCTION pgokf.concept_search_hybrid(text, real[], bigint, integer) TO pgokf_reader;
COMMENT ON FUNCTION pgokf.concept_search_hybrid(text, real[], bigint, integer) IS
    'Hybrid search: Reciprocal Rank Fusion (RRF, k=60) of the lexical result of query (via the configured search_backend) and the semantic result of query_embedding, fused entirely in SQL (rank = fused RRF score). Reader-level, invoker rights; enabled bundles only; limit_count in 1..=500. Degrades to lexical-only with a WARNING when pgvector is not installed.';

-- F5: HNSW index management. Admin-tier, SECURITY DEFINER (owns the DDL on the
-- extension-owned embedding table).
CREATE FUNCTION pgokf."rebuild_embedding_index"() RETURNS bool
STRICT
LANGUAGE c
AS 'MODULE_PATHNAME', 'rebuild_embedding_index_wrapper';

ALTER FUNCTION pgokf.rebuild_embedding_index()
    SECURITY DEFINER SET search_path = pg_catalog, pg_temp;
REVOKE ALL ON FUNCTION pgokf.rebuild_embedding_index() FROM PUBLIC;
GRANT EXECUTE ON FUNCTION pgokf.rebuild_embedding_index() TO pgokf_admin;
COMMENT ON FUNCTION pgokf.rebuild_embedding_index() IS
    'Admin-only. (Re)build the pgvector HNSW (cosine) index on pgokf.concept_embedding for the configured embedding_dim; returns true when built, or false (with a NOTICE) when pgvector is absent or embedding_dim exceeds pgvector''s 2000-dimension HNSW limit.';
