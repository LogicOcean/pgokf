-- SPDX-License-Identifier: AGPL-3.0-only
-- pgokf extension upgrade: 0.2.0 -> 0.3.0
--
-- Never DROP, TRUNCATE, DELETE, or rewrite existing catalog data in an upgrade
-- script: doing so would break the no-data-loss guarantee asserted by the
-- api_stability upgrade tests.

-- ===========================================================================
-- Stale-embedding fix (independent capability; splice-safe section).
--
-- 0.2.0 never invalidated an embedding when its concept's text changed: the
-- companion embedder polls only concepts with NO embedding row, so an updated
-- concept kept ranking by a vector computed from its old text. The fix is
-- behavioral and lives in the 0.3.0 shared library: the sync engine deletes a
-- re-written concept's embedding row in the same transaction as the concept
-- upsert (removed/re-identified concepts already cascade through the foreign
-- key), so on commit no vector can coexist with text it was not computed
-- from, and the unchanged missing-row poll re-embeds the concept.
--
-- The one new SQL object is a four-argument overload of
-- pgokf.set_concept_embedding: a compare-and-set guard that closes the
-- inference race. Inference is slow and concurrent with syncs, so an
-- unguarded writer could re-insert a vector computed from the old text AFTER
-- a sync deleted it. The overload locks the concept row and refuses the write
-- with SQLSTATE 40001 (retryable) unless the caller's expected_file_hash
-- still equals the concept's current file_hash. No table, column, or index
-- changes: semantic/hybrid search needs no staleness predicate because a
-- stored vector now always matches its concept's current text.
--
-- Declared exactly as the fresh 0.3.0 install script declares it (STRICT,
-- C-language wrapper exported by the 0.3.0 shared library), then hardened as
-- the embedding_function_hardening block hardens it. The three-argument form
-- is untouched. A catalog upgraded with this section is identical to a fresh
-- 0.3.0 install for this capability.
-- ===========================================================================
CREATE FUNCTION pgokf."set_concept_embedding"(
    "bundle_id" bigint,
    "concept_id" text,
    "embedding" real[],
    "expected_file_hash" text
) RETURNS void
STRICT
LANGUAGE c
AS 'MODULE_PATHNAME', 'set_concept_embedding_if_current_wrapper';

ALTER FUNCTION pgokf.set_concept_embedding(bigint, text, real[], text)
    SECURITY DEFINER SET search_path = pg_catalog, pg_temp;
REVOKE ALL ON FUNCTION pgokf.set_concept_embedding(bigint, text, real[], text) FROM PUBLIC;
GRANT EXECUTE ON FUNCTION pgokf.set_concept_embedding(bigint, text, real[], text) TO pgokf_writer;
COMMENT ON FUNCTION pgokf.set_concept_embedding(bigint, text, real[], text) IS
    'Store or replace one concept''s embedding (real[]), guarded against a concurrent sync: the concept row is locked and the write is refused with 40001 (retryable - re-read the concept and re-embed) unless expected_file_hash still equals the concept''s current file_hash, so a vector computed from superseded text can never be stored. Writer-tier (pgokf_writer; admin inherits it), SECURITY DEFINER. Validates the concept exists and len(embedding)=embedding_dim (else 22023) and upserts.';
