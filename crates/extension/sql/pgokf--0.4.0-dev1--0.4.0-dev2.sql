-- SPDX-License-Identifier: AGPL-3.0-only
-- pgokf extension upgrade: 0.4.0-dev1 -> 0.4.0-dev2
--
-- Search-function catalog reconciliation. A field catalog that followed an
-- unpublished early 0.3.0-dev build reached 0.3.0-dev2 (and so, through the
-- ordinary receipt-only edges, 0.4.0-dev1) without section 13 of the
-- 0.2.0 -> 0.3.0-dev script. Compared with a fresh install it carries:
--
--   * the retired 0.2.0 overloads
--     pgokf.concept_search_semantic(real[], bigint, integer) and
--     pgokf.concept_search_hybrid(text, real[], bigint, integer) next to the
--     widened concept_types overloads. Their defaults overlap, so every call
--     that omits concept_types - and even an explicitly typed old-arity
--     call - is ambiguous (42725);
--   * the retired 11-argument pgokf.bm25_hits helper instead of the
--     12-argument one the concept_search BM25 path calls (p_concept_types);
--   * NULL ACLs (PostgreSQL's default PUBLIC EXECUTE), no comment and no
--     extension initial privileges on the two current search overloads;
--   * superseded comments on pgokf.get_skill, get_script and get_reference.
--
-- No earlier edge can be edited in place (0.4.0-dev1 is already reviewed and
-- may be installed), so this ordinary development step reconciles the
-- catalog, and a catalog in either state - factory or drifted - ends
-- object-for-object equal to a fresh 0.4.0-dev2 install.
--
-- Safety contract. Only supported extension DDL is used - DROP FUNCTION,
-- CREATE OR REPLACE FUNCTION, ALTER FUNCTION OWNER, REVOKE/GRANT and
-- COMMENT inside the extension update - never a direct system-catalog write;
-- PostgreSQL itself records the extension initial privileges (pg_init_privs)
-- for the REVOKE/GRANT statements it runs inside ALTER EXTENSION, and
-- removes those of a dropped function. A retired object is dropped only in
-- its exact published 0.2.0 shape (membership, language, argument names and
-- defaults, result, C symbol and library or body digest, volatility and
-- parallel safety, security, configuration, owner, ACL) and only when no
-- catalog-recorded object depends on it; the drops are RESTRICT, never
-- CASCADE. Any other shape, an unknown overload of these names, or a custom
-- grant on a current search function aborts the whole update with a named
-- diagnostic and changes nothing, so an ordinary ALTER EXTENSION pgokf UPDATE
-- is retried after deliberate review - there is no force flag. Where the
-- retired objects are absent and the current ones are canonical (every
-- published and fresh route), every statement below is a no-op.
--
-- Never DROP, TRUNCATE, DELETE, or rewrite existing catalog data in an upgrade
-- script: doing so would break the no-data-loss guarantee asserted by the
-- api_stability upgrade tests. The functions dropped here carry no data.

-- ===========================================================================
-- 1. Refuse anything but the known shapes, before any change.
-- ===========================================================================
DO $dev2_reconciliation_guard$
DECLARE
    v_extension pg_catalog.oid;
    v_owner pg_catalog.oid;
    v_reader pg_catalog.oid := 'pgokf_reader'::pg_catalog.regrole::pg_catalog.oid;
    v_function pg_catalog.oid;
    v_problem text;
    v_detail text;
    v_retired record;
    v_current record;
BEGIN
    SELECT oid, extowner INTO STRICT v_extension, v_owner
    FROM pg_catalog.pg_extension WHERE extname = 'pgokf';

    -- An overload of these names that is neither current nor a known retired
    -- shape would stay ambiguous or unexplained; refuse it outright.
    SELECT pg_catalog.string_agg(pg_catalog.format('pgokf.%s(%s)', p.proname,
               pg_catalog.pg_get_function_identity_arguments(p.oid)), ', ' ORDER BY 1)
    INTO v_detail
    FROM pg_catalog.pg_proc AS p
    WHERE p.pronamespace = 'pgokf'::pg_catalog.regnamespace
      AND p.proname IN ('concept_search_semantic', 'concept_search_hybrid', 'bm25_hits')
      AND NOT EXISTS (
          SELECT 1 FROM pg_catalog.unnest(ARRAY[
              'pgokf.concept_search_semantic(real[], bigint, integer, text[])',
              'pgokf.concept_search_hybrid(text, real[], bigint, integer, text[])',
              'pgokf.bm25_hits(text, bigint, bigint, text, text, text[], text, text, real, bigint, text, text[])',
              'pgokf.concept_search_semantic(real[], bigint, integer)',
              'pgokf.concept_search_hybrid(text, real[], bigint, integer)',
              'pgokf.bm25_hits(text, bigint, bigint, text, text, text[], text, text, real, bigint, text)'
          ]) AS known(signature)
          WHERE pg_catalog.to_regprocedure(known.signature) = p.oid);
    IF v_detail IS NOT NULL THEN
        RAISE EXCEPTION 'pgokf 0.4.0-dev2 reconciliation refuses unknown search overloads: %', v_detail
            USING ERRCODE = 'object_not_in_prerequisite_state',
                  HINT = 'Review and remove the unknown overloads deliberately, then retry ALTER EXTENSION pgokf UPDATE. Nothing was changed.';
    END IF;

    -- The retired 0.2.0 overloads: exact published shape or refuse.
    FOR v_retired IN
        SELECT * FROM (VALUES
            ('pgokf.concept_search_semantic(real[], bigint, integer)',
             'query_embedding real[], bundle_id bigint DEFAULT NULL::bigint, limit_count integer DEFAULT 10',
             'SETOF pgokf.concept_search_result',
             'c', 'concept_search_semantic_wrapper', NULL::text,
             's', 's', false, NULL::text[]),
            ('pgokf.concept_search_hybrid(text, real[], bigint, integer)',
             'query text, query_embedding real[], bundle_id bigint DEFAULT NULL::bigint, limit_count integer DEFAULT 10',
             'SETOF pgokf.concept_search_result',
             'c', 'concept_search_hybrid_wrapper', NULL::text,
             's', 'r', false, NULL::text[]),
            ('pgokf.bm25_hits(text, bigint, bigint, text, text, text[], text, text, real, bigint, text)',
             'p_query text, p_bundle_id bigint, p_limit bigint, p_text_search_config text, p_concept_type text, p_tags text[], p_status text, p_trust_tier text, p_after_rank real, p_after_bundle_id bigint, p_after_concept_id text',
             'TABLE(bundle_id bigint, concept_id text, path text, title text, type text, rank real, headline text)',
             'plpgsql', NULL::text, '1b9dab9b527f30d0d3cd55c14a81e26d',
             's', 's', true, ARRAY['search_path=pg_catalog, pg_temp'])
        ) AS retired(signature, arguments, result, language, symbol, body_md5,
                     volatility, parallel, security_definer, config)
    LOOP
        v_function := pg_catalog.to_regprocedure(v_retired.signature);
        IF v_function IS NULL THEN
            CONTINUE;
        END IF;
        v_problem := NULL;
        v_detail := NULL;
        IF NOT EXISTS (
            SELECT 1 FROM pg_catalog.pg_depend AS d
            WHERE d.classid = 'pg_catalog.pg_proc'::pg_catalog.regclass
              AND d.objid = v_function
              AND d.refclassid = 'pg_catalog.pg_extension'::pg_catalog.regclass
              AND d.refobjid = v_extension
              AND d.deptype = 'e'
        ) THEN
            v_problem := 'extension membership (it is not a member of extension pgokf)';
        ELSIF (SELECT l.lanname FROM pg_catalog.pg_proc AS p
               JOIN pg_catalog.pg_language AS l ON l.oid = p.prolang
               WHERE p.oid = v_function) IS DISTINCT FROM v_retired.language
           OR (SELECT p.prokind FROM pg_catalog.pg_proc AS p WHERE p.oid = v_function) <> 'f' THEN
            v_problem := pg_catalog.format('language (expected a %s function)', v_retired.language);
        ELSIF pg_catalog.pg_get_function_arguments(v_function) IS DISTINCT FROM v_retired.arguments
           OR pg_catalog.pg_get_function_result(v_function) IS DISTINCT FROM v_retired.result THEN
            v_problem := 'argument names, defaults or result type';
        ELSIF v_retired.symbol IS NOT NULL AND NOT EXISTS (
                SELECT 1 FROM pg_catalog.pg_proc AS p
                WHERE p.oid = v_function AND p.prosrc = v_retired.symbol
                  AND p.probin = '$libdir/pgokf') THEN
            v_problem := pg_catalog.format('C entry point (expected symbol %s in $libdir/pgokf)', v_retired.symbol);
        ELSIF v_retired.body_md5 IS NOT NULL AND NOT EXISTS (
                SELECT 1 FROM pg_catalog.pg_proc AS p
                WHERE p.oid = v_function AND pg_catalog.md5(p.prosrc) = v_retired.body_md5
                  AND p.probin IS NULL) THEN
            v_problem := pg_catalog.format('body (expected the published 0.2.0 body, md5 %s)', v_retired.body_md5);
        ELSIF NOT EXISTS (
                SELECT 1 FROM pg_catalog.pg_proc AS p
                WHERE p.oid = v_function AND p.provolatile = v_retired.volatility
                  AND p.proparallel = v_retired.parallel
                  AND NOT p.proisstrict AND NOT p.proleakproof) THEN
            v_problem := 'volatility, parallel safety, strictness or leakproofness';
        ELSIF (SELECT p.prosecdef FROM pg_catalog.pg_proc AS p WHERE p.oid = v_function)
              IS DISTINCT FROM v_retired.security_definer THEN
            v_problem := pg_catalog.format('security (expected SECURITY %s)',
                CASE WHEN v_retired.security_definer THEN 'DEFINER' ELSE 'INVOKER' END);
        ELSIF (SELECT p.proconfig FROM pg_catalog.pg_proc AS p WHERE p.oid = v_function)
              IS DISTINCT FROM v_retired.config THEN
            v_problem := pg_catalog.format('configuration (expected %s)', COALESCE(v_retired.config::text, 'none'));
        ELSIF (SELECT p.proowner FROM pg_catalog.pg_proc AS p WHERE p.oid = v_function) <> v_owner THEN
            v_problem := pg_catalog.format('owner (expected the extension owner %s)', v_owner::pg_catalog.regrole);
        ELSIF (SELECT pg_catalog.array_agg(
                   pg_catalog.format('%s:%s:%s:%s', a.grantor, a.grantee, a.privilege_type,
                                     a.is_grantable::text) ORDER BY a.grantee)
               FROM pg_catalog.pg_proc AS p, pg_catalog.aclexplode(p.proacl) AS a
               WHERE p.oid = v_function)
              IS DISTINCT FROM
              (SELECT pg_catalog.array_agg(pg_catalog.format('%s:%s:EXECUTE:false', v_owner, g) ORDER BY g)
               FROM (SELECT v_owner AS g UNION SELECT v_reader) AS expected) THEN
            v_problem := pg_catalog.format('ACL (expected EXECUTE for exactly the owner %s and pgokf_reader, owner grantor, no grant options)',
                v_owner::pg_catalog.regrole);
        ELSE
            SELECT pg_catalog.string_agg(pg_catalog.pg_describe_object(d.classid, d.objid, d.objsubid), ', ' ORDER BY 1)
            INTO v_detail
            FROM pg_catalog.pg_depend AS d
            WHERE d.refclassid = 'pg_catalog.pg_proc'::pg_catalog.regclass
              AND d.refobjid = v_function;
            IF v_detail IS NOT NULL THEN
                v_problem := pg_catalog.format('dependents (%s)', v_detail);
            END IF;
        END IF;
        IF v_problem IS NOT NULL THEN
            RAISE EXCEPTION 'pgokf 0.4.0-dev2 reconciliation refuses retired function %: unexpected %',
                v_retired.signature, v_problem
                USING ERRCODE = 'object_not_in_prerequisite_state',
                      DETAIL = 'Only the exact published 0.2.0 definition, an extension member owned by the extension owner with owner plus pgokf_reader EXECUTE and no dependents, is dropped automatically.',
                      HINT = 'Review the object and restore or remove it deliberately, then retry ALTER EXTENSION pgokf UPDATE. Nothing was changed.';
        END IF;
    END LOOP;

    -- The current search functions: members owned by the extension owner,
    -- with either PostgreSQL's default ACL (the recorded drift) or the
    -- canonical one. Custom grants are never silently revoked.
    FOR v_current IN
        SELECT * FROM (VALUES
            ('pgokf.concept_search_semantic(real[], bigint, integer, text[])', true),
            ('pgokf.concept_search_hybrid(text, real[], bigint, integer, text[])', true),
            ('pgokf.bm25_hits(text, bigint, bigint, text, text, text[], text, text, real, bigint, text, text[])', false)
        ) AS current_functions(signature, required)
    LOOP
        v_function := pg_catalog.to_regprocedure(v_current.signature);
        IF v_function IS NULL THEN
            IF v_current.required THEN
                RAISE EXCEPTION 'pgokf 0.4.0-dev2 reconciliation refuses a catalog without %', v_current.signature
                    USING ERRCODE = 'object_not_in_prerequisite_state',
                          HINT = 'The function is part of every 0.4.0-dev1 installation; restore it deliberately, then retry ALTER EXTENSION pgokf UPDATE. Nothing was changed.';
            END IF;
            CONTINUE;
        END IF;
        IF NOT EXISTS (
            SELECT 1 FROM pg_catalog.pg_depend AS d
            WHERE d.classid = 'pg_catalog.pg_proc'::pg_catalog.regclass
              AND d.objid = v_function
              AND d.refclassid = 'pg_catalog.pg_extension'::pg_catalog.regclass
              AND d.refobjid = v_extension
              AND d.deptype = 'e'
        ) THEN
            RAISE EXCEPTION 'pgokf 0.4.0-dev2 reconciliation refuses current function %: unexpected extension membership (it is not a member of extension pgokf)',
                v_current.signature
                USING ERRCODE = 'object_not_in_prerequisite_state',
                      HINT = 'Review the object and restore or remove it deliberately, then retry ALTER EXTENSION pgokf UPDATE. Nothing was changed.';
        END IF;
        IF NOT EXISTS (
            SELECT 1 FROM pg_catalog.pg_proc AS p
            WHERE p.oid = v_function AND p.proowner = v_owner
              AND (p.proacl IS NULL
                   OR (SELECT pg_catalog.array_agg(
                              pg_catalog.format('%s:%s:%s:%s', a.grantor, a.grantee,
                                                a.privilege_type, a.is_grantable::text) ORDER BY a.grantee)
                       FROM pg_catalog.aclexplode(p.proacl) AS a)
                      = (SELECT pg_catalog.array_agg(pg_catalog.format('%s:%s:EXECUTE:false', v_owner, g) ORDER BY g)
                         FROM (SELECT v_owner AS g UNION SELECT v_reader) AS expected))
        ) THEN
            RAISE EXCEPTION 'pgokf 0.4.0-dev2 reconciliation refuses current function %: unexpected owner or ACL (expected owner % with either the default ACL or EXECUTE for exactly the owner and pgokf_reader)',
                v_current.signature, v_owner::pg_catalog.regrole
                USING ERRCODE = 'object_not_in_prerequisite_state',
                      HINT = 'Custom grants are never silently revoked. Review ownership and explicit ACLs and restore canonical metadata deliberately, then retry ALTER EXTENSION pgokf UPDATE. Nothing was changed.';
        END IF;
    END LOOP;
END
$dev2_reconciliation_guard$;

-- ===========================================================================
-- 2. Drop the retired overloads (verified above; RESTRICT, never CASCADE).
-- ===========================================================================
DROP FUNCTION IF EXISTS pgokf.concept_search_semantic(real[], bigint, integer) RESTRICT;
DROP FUNCTION IF EXISTS pgokf.concept_search_hybrid(text, real[], bigint, integer) RESTRICT;
DROP FUNCTION IF EXISTS pgokf.bm25_hits(text, bigint, bigint, text, text, text[], text, text, real, bigint, text) RESTRICT;

-- ===========================================================================
-- 3. The current BM25 helper: the bm25_hits_function block of
-- src/catalog/search_backend.rs, verbatim except for CREATE OR REPLACE (a
-- no-op where it already exists; api_stability keeps the two identical).
-- plpgsql resolves the paradedb references only on execution, so this is
-- harmless where pg_search is absent. A helper created here belongs to the
-- extension owner, as on a fresh install, whoever runs the update.
-- ===========================================================================
CREATE OR REPLACE FUNCTION pgokf.bm25_hits(
    p_query text,
    p_bundle_id bigint,
    p_limit bigint,
    p_text_search_config text,
    p_concept_type text,
    p_tags text[],
    p_status text,
    p_trust_tier text,
    p_after_rank real,
    p_after_bundle_id bigint,
    p_after_concept_id text,
    p_concept_types text[])
RETURNS TABLE (
    bundle_id bigint,
    concept_id text,
    path text,
    title text,
    type text,
    rank real,
    headline text)
LANGUAGE plpgsql
STABLE PARALLEL SAFE
SECURITY DEFINER
SET search_path = pg_catalog, pg_temp
AS $fn$
DECLARE
    -- Resolved once, up front, and bound into the query as a parameter: the
    -- policies inline current_setting() directly, but pg_search 0.25 cannot
    -- plan its scan under a predicate that calls a function (that inline
    -- form is exactly what row-level security injects for non-owners, and
    -- exactly what the Unsupported-query-shape error was about). Empty = unset.
    v_tenant text := NULLIF(pg_catalog.current_setting('pgokf.tenant', true), '');
BEGIN
    -- The policies' rule, applied here because this body bypasses them: an
    -- unscoped session sees nothing when the catalog requires a tenant.
    IF v_tenant IS NULL AND pgokf.tenant_required() THEN
        RETURN;
    END IF;
    RETURN QUERY
    SELECT hits.bundle_id,
           hits.concept_id,
           hits.path,
           hits.title,
           hits.type,
           hits.rank,
           hits.headline
    FROM (
        SELECT c.bundle_id AS bundle_id,
               c.id AS concept_id,
               c.path AS path,
               c.title AS title,
               c.type AS type,
               paradedb.score(c) AS rank,
               pg_catalog.ts_headline(
                   p_text_search_config::pg_catalog.regconfig,
                   pg_catalog.concat_ws(' ', c.title, c.description, c.body_text),
                   pg_catalog.websearch_to_tsquery(p_text_search_config::pg_catalog.regconfig, p_query)) AS headline
        FROM pgokf.concepts c
        JOIN pgokf.bundles b ON b.id = c.bundle_id AND b.enabled AND b.retired_at IS NULL
        LEFT JOIN pgokf.concept_provenance cp
               ON cp.bundle_id = c.bundle_id AND cp.concept_id = c.id
        WHERE c.id @@@ paradedb.boolean(should => ARRAY[
                  paradedb.match('title', p_query),
                  paradedb.match('description', p_query),
                  paradedb.match('body_text', p_query)])
          AND (v_tenant IS NULL OR c.tenant_id = v_tenant)
          AND (p_bundle_id IS NULL OR c.bundle_id = p_bundle_id)
          AND (p_concept_type IS NULL OR c.type = p_concept_type)
          AND (p_tags IS NULL OR c.tags @> p_tags)
          AND (p_status IS NULL OR cp.status = p_status)
          AND (p_trust_tier IS NULL OR cp.trust_tier = p_trust_tier)
          AND (p_concept_types IS NULL OR c.type = ANY(p_concept_types))
    ) AS hits
    WHERE p_after_rank IS NULL
       OR hits.rank < p_after_rank
       OR (hits.rank = p_after_rank AND hits.bundle_id > p_after_bundle_id)
       OR (hits.rank = p_after_rank AND hits.bundle_id = p_after_bundle_id AND hits.concept_id > p_after_concept_id)
    ORDER BY hits.rank DESC, hits.bundle_id ASC, hits.concept_id ASC
    LIMIT p_limit;
END
$fn$;

DO $dev2_helper_owner$
DECLARE
    v_owner pg_catalog.oid;
BEGIN
    SELECT extowner INTO STRICT v_owner FROM pg_catalog.pg_extension WHERE extname = 'pgokf';
    IF (SELECT proowner FROM pg_catalog.pg_proc
        WHERE oid = 'pgokf.bm25_hits(text, bigint, bigint, text, text, text[], text, text, real, bigint, text, text[])'::pg_catalog.regprocedure)
       <> v_owner THEN
        EXECUTE pg_catalog.format(
            'ALTER FUNCTION pgokf.bm25_hits(text, bigint, bigint, text, text, text[], text, text, real, bigint, text, text[]) OWNER TO %s',
            v_owner::pg_catalog.regrole);
    END IF;
END
$dev2_helper_owner$;

REVOKE ALL ON FUNCTION pgokf.bm25_hits(text, bigint, bigint, text, text, text[], text, text, real, bigint, text, text[]) FROM PUBLIC;
GRANT EXECUTE ON FUNCTION pgokf.bm25_hits(text, bigint, bigint, text, text, text[], text, text, real, bigint, text, text[]) TO pgokf_reader;
COMMENT ON FUNCTION pgokf.bm25_hits(text, bigint, bigint, text, text, text[], text, text, real, bigint, text, text[]) IS
    'Internal helper behind concept_search when search_backend = bm25 resolves to the ParadeDB pg_search provider (the pg_textsearch provider runs inline with invoker rights and does not use it); not part of the stable API. Runs the ParadeDB pg_search BM25 hit query with the owner''s privileges (row-level security wraps the catalog tables in a shape pg_search cannot plan for non-owners) while applying the same pgokf.tenant scoping the policies enforce, over active bundles only, with concept_search''s filters (p_concept_types is the type-membership form concept_search_hybrid uses to constrain the lexical candidate list before truncation), keyset cursor, and limit. Reader-level; returns exactly the rows concept_search would.';

-- ===========================================================================
-- 4. Least-privilege ACLs and comments of the current search overloads (the
-- statements of the embedding_function_hardening block of
-- src/catalog/embedding.rs, verbatim). Inside the extension update
-- PostgreSQL records the resulting ACLs as extension initial privileges.
-- ===========================================================================
REVOKE ALL ON FUNCTION pgokf.concept_search_semantic(real[], bigint, integer, text[]) FROM PUBLIC;
REVOKE ALL ON FUNCTION pgokf.concept_search_hybrid(text, real[], bigint, integer, text[]) FROM PUBLIC;
GRANT EXECUTE ON FUNCTION pgokf.concept_search_semantic(real[], bigint, integer, text[]) TO pgokf_reader;
GRANT EXECUTE ON FUNCTION pgokf.concept_search_hybrid(text, real[], bigint, integer, text[]) TO pgokf_reader;
COMMENT ON FUNCTION pgokf.concept_search_semantic(real[], bigint, integer, text[]) IS
    'Semantic nearest-neighbor search: rank concepts by pgvector cosine distance to query_embedding (rank = normalized cosine similarity). Reader-level, invoker rights; active bundles only. query_embedding must have embedding_dim dimensions; limit_count in 1..=500. Requires pgvector: raises 22023 naming the missing dependency when it is not installed (no lexical fallback). Only ELIGIBLE embeddings rank: source_file_hash equal to the concept''s current file_hash, model/dimension/contract matching the embedding_model/embedding_dim/embedding_contract policy, and the concept effectively fresh (bundle freshness fresh, no covering concept/path override); a stale or legacy (NULL-provenance) row never ranks even while the HNSW index physically retains it. concept_types is an optional type-membership filter (a hit''s type must be one of the listed types; NULL or empty is no filter) applied inside the ranked query BEFORE the candidate list is truncated to limit_count, so a filtered call returns up to limit_count eligible hits of the selected types even when excluded types outrank them.';
COMMENT ON FUNCTION pgokf.concept_search_hybrid(text, real[], bigint, integer, text[]) IS
    'Hybrid search: Reciprocal Rank Fusion (RRF, k=60) of the lexical result of query (via the configured search_backend) and the semantic result of query_embedding, fused entirely in SQL (rank = fused RRF score). Reader-level, invoker rights; enabled bundles only; limit_count in 1..=500. The semantic component ranks eligible (current, fresh) embeddings only, so an ineligible vector never leaks into the fused result; the lexical component may still return a stale concept, labeled by pgokf.concept_search_fresh. Degrades to lexical-only with a WARNING when pgvector is not installed. concept_types is an optional type-membership filter (a hit''s type must be one of the listed types; NULL or empty is no filter) applied to BOTH ranked inputs before either is truncated to limit_count, so the fused page is exactly the type-filtered top-limit_count, never underfilled by higher-ranked excluded types.';

-- ===========================================================================
-- 5. Current package-reader comments (the package_function_hardening block of
-- src/catalog/packages.rs, verbatim).
-- ===========================================================================
COMMENT ON FUNCTION pgokf.get_skill(bigint, text) IS
    'Return one Agent Skills package as pgokf.skill_result: name, description, package directory and hash, visibility, the complete original frontmatter, the exact SKILL.md bytes, and the JSON listing of its scripts, references, and assets. Reader-level, tenant-scoped, and audited: each successful read appends a get_skill row to the access log. Raises 22023 for an unknown skill.';
COMMENT ON FUNCTION pgokf.get_script(bigint, text) IS
    'Return one package script as pgokf.script_result: language, package-relative path and owning skill, size, SHA-256, declared runtime/arguments/exit codes when known, and the exact stored bytes (never body_text). Reader-level, tenant-scoped, and audited: each successful read appends a get_script row to the access log. Raises 22023 for an unknown script.';
COMMENT ON FUNCTION pgokf.get_reference(bigint, text, boolean) IS
    'Return one package reference or asset as pgokf.reference_result: format and media type, package-relative path and owning skill, size, SHA-256, its text when textual, and the exact stored bytes when include_bytes (the default) is true. Reader-level, tenant-scoped, and audited: each successful read appends a get_reference row to the access log (detail metadata when the bytes were not requested). Raises 22023 for an unknown reference.';

-- Last, so any relation a script adds is registered for pg_dump (none here;
-- the rule holds for every upgrade script).
SELECT pgokf_private.register_dump_relations();
