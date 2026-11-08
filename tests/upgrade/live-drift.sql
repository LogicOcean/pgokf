-- SPDX-License-Identifier: AGPL-3.0-only
-- Reproduce the complete drift recorded on a field catalog that followed an
-- unpublished early 0.3.0-dev build (applied at 0.3.0-dev2 or later, outside
-- the update machinery). Every definition and comment is public: the retired
-- overloads are the published 0.2.0 ones (0.1.5 -> 0.1.6, with
-- 0.1.14 -> 0.1.15 hybrid PARALLEL RESTRICTED; the 0.1.16 bm25_hits body)
-- carrying their 0.3.0-dev / 0.1.15 comments, the superseded package comments
-- are the 0.2.0 development wording, and the superseded pgokf_web comments are
-- the wording of the first development builds of the web identity tables.
-- The result is the drift pgokf--0.4.0-dev1--0.4.0-dev2.sql and
-- pgokf--0.4.0-dev2--0.4.0-dev3.sql reconcile, and the declared column order
-- neither rebuilds (tests/upgrade/catalog-exceptions.json):
--   * retired concept_search_semantic/hybrid overloads next to the current
--     ones (ambiguous defaulted calls) and the retired 11-argument bm25_hits
--     instead of the 12-argument helper, all extension members with the
--     owner plus pgokf_reader grants and matching initial privileges;
--   * current search overloads with PostgreSQL's default ACL, no comment and
--     no extension initial privileges;
--   * superseded get_skill/get_script/get_reference comments;
--   * pgokf_web.users with display_name and provider appended after
--     updated_at, and pgokf_web.sessions with provider appended after
--     created_at (so their row types too), every constraint, grant, index and
--     row kept - the shape a hand migration adding those columns leaves;
--   * superseded comments on the pgokf_web schema, the pgokf_web.users table
--     and its name, role, password_hash, created_at and updated_at columns.
-- scripts/upgrade-parity.py checks that this fixture differs from a factory
-- catalog in exactly these objects, so an incomplete fixture fails the gate.

-- Current overloads: detach (dropping their initial privileges), re-create
-- with the generated definition and the default ACL, re-attach (no
-- initial privileges are recorded for a NULL ACL), no comment.
ALTER EXTENSION pgokf DROP FUNCTION pgokf.concept_search_semantic(real[], bigint, integer, text[]);
DROP FUNCTION pgokf.concept_search_semantic(real[], bigint, integer, text[]);
CREATE FUNCTION pgokf."concept_search_semantic"(
    "query_embedding" real[],
    "bundle_id" bigint DEFAULT NULL,
    "limit_count" INT DEFAULT 10,
    "concept_types" TEXT[] DEFAULT NULL
) RETURNS SETOF pgokf.concept_search_result
STABLE PARALLEL SAFE
LANGUAGE c
AS '$libdir/pgokf', 'concept_search_semantic_wrapper';
ALTER EXTENSION pgokf ADD FUNCTION pgokf.concept_search_semantic(real[], bigint, integer, text[]);

ALTER EXTENSION pgokf DROP FUNCTION pgokf.concept_search_hybrid(text, real[], bigint, integer, text[]);
DROP FUNCTION pgokf.concept_search_hybrid(text, real[], bigint, integer, text[]);
CREATE FUNCTION pgokf."concept_search_hybrid"(
    "query" TEXT,
    "query_embedding" real[],
    "bundle_id" bigint DEFAULT NULL,
    "limit_count" INT DEFAULT 10,
    "concept_types" TEXT[] DEFAULT NULL
) RETURNS SETOF pgokf.concept_search_result
STABLE PARALLEL RESTRICTED
LANGUAGE c
AS '$libdir/pgokf', 'concept_search_hybrid_wrapper';
ALTER EXTENSION pgokf ADD FUNCTION pgokf.concept_search_hybrid(text, real[], bigint, integer, text[]);

-- Retired 0.2.0 overloads, as members with their published grants.
CREATE FUNCTION pgokf."concept_search_semantic"(
    "query_embedding" real[],
    "bundle_id" bigint DEFAULT NULL,
    "limit_count" integer DEFAULT 10
) RETURNS SETOF pgokf.concept_search_result
STABLE PARALLEL SAFE
LANGUAGE c
AS '$libdir/pgokf', 'concept_search_semantic_wrapper';
REVOKE ALL ON FUNCTION pgokf.concept_search_semantic(real[], bigint, integer) FROM PUBLIC;
GRANT EXECUTE ON FUNCTION pgokf.concept_search_semantic(real[], bigint, integer) TO pgokf_reader;
COMMENT ON FUNCTION pgokf.concept_search_semantic(real[], bigint, integer) IS
    'Semantic nearest-neighbor search: rank concepts by pgvector cosine distance to query_embedding (rank = normalized cosine similarity). Reader-level, invoker rights; active bundles only. query_embedding must have embedding_dim dimensions; limit_count in 1..=500. Requires pgvector: raises 22023 naming the missing dependency when it is not installed (no lexical fallback). Only ELIGIBLE embeddings rank: source_file_hash equal to the concept''s current file_hash, model/dimension/contract matching the embedding_model/embedding_dim/embedding_contract policy, and the concept effectively fresh (bundle freshness fresh, no covering concept/path override); a stale or legacy (NULL-provenance) row never ranks even while the HNSW index physically retains it.';
ALTER EXTENSION pgokf ADD FUNCTION pgokf.concept_search_semantic(real[], bigint, integer);

CREATE FUNCTION pgokf."concept_search_hybrid"(
    "query" text,
    "query_embedding" real[],
    "bundle_id" bigint DEFAULT NULL,
    "limit_count" integer DEFAULT 10
) RETURNS SETOF pgokf.concept_search_result
STABLE PARALLEL RESTRICTED
LANGUAGE c
AS '$libdir/pgokf', 'concept_search_hybrid_wrapper';
REVOKE ALL ON FUNCTION pgokf.concept_search_hybrid(text, real[], bigint, integer) FROM PUBLIC;
GRANT EXECUTE ON FUNCTION pgokf.concept_search_hybrid(text, real[], bigint, integer) TO pgokf_reader;
COMMENT ON FUNCTION pgokf.concept_search_hybrid(text, real[], bigint, integer) IS
    'Hybrid search: Reciprocal Rank Fusion (RRF, k=60) of the lexical result of query (via the configured search_backend) and the semantic result of query_embedding, fused entirely in SQL (rank = fused RRF score). Reader-level, invoker rights; enabled bundles only; limit_count in 1..=500. The semantic component ranks eligible (current, fresh) embeddings only, so an ineligible vector never leaks into the fused result; the lexical component may still return a stale concept, labeled by pgokf.concept_search_fresh. Degrades to lexical-only with a WARNING when pgvector is not installed.';
ALTER EXTENSION pgokf ADD FUNCTION pgokf.concept_search_hybrid(text, real[], bigint, integer);

-- The retired 11-argument helper replaces the 12-argument one.
ALTER EXTENSION pgokf DROP FUNCTION pgokf.bm25_hits(text, bigint, bigint, text, text, text[], text, text, real, bigint, text, text[]);
DROP FUNCTION pgokf.bm25_hits(text, bigint, bigint, text, text, text[], text, text, real, bigint, text, text[]);
CREATE FUNCTION pgokf.bm25_hits(
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
    p_after_concept_id text)
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
    ) AS hits
    WHERE p_after_rank IS NULL
       OR hits.rank < p_after_rank
       OR (hits.rank = p_after_rank AND hits.bundle_id > p_after_bundle_id)
       OR (hits.rank = p_after_rank AND hits.bundle_id = p_after_bundle_id AND hits.concept_id > p_after_concept_id)
    ORDER BY hits.rank DESC, hits.bundle_id ASC, hits.concept_id ASC
    LIMIT p_limit;
END
$fn$;
REVOKE ALL ON FUNCTION pgokf.bm25_hits(text, bigint, bigint, text, text, text[], text, text, real, bigint, text) FROM PUBLIC;
GRANT EXECUTE ON FUNCTION pgokf.bm25_hits(text, bigint, bigint, text, text, text[], text, text, real, bigint, text) TO pgokf_reader;
COMMENT ON FUNCTION pgokf.bm25_hits(text, bigint, bigint, text, text, text[], text, text, real, bigint, text) IS
    'Internal helper behind concept_search when search_backend = bm25 resolves to the ParadeDB pg_search provider (the pg_textsearch provider runs inline with invoker rights and does not use it); not part of the stable API. Runs the ParadeDB pg_search BM25 hit query with the owner''s privileges (row-level security wraps the catalog tables in a shape pg_search cannot plan for non-owners) while applying the same pgokf.tenant scoping the policies enforce, over active bundles only, with concept_search''s filters, keyset cursor, and limit. Reader-level; returns exactly the rows concept_search would.';
ALTER EXTENSION pgokf ADD FUNCTION pgokf.bm25_hits(text, bigint, bigint, text, text, text[], text, text, real, bigint, text);

-- Superseded package-reader comments.
COMMENT ON FUNCTION pgokf.get_skill(bigint, text) IS
    'Return one Agent Skills package as pgokf.skill_result: name, description, package directory and hash, visibility, the complete original frontmatter, the exact SKILL.md bytes, and the JSON listing of its scripts, references, and assets. Reader-level, tenant-scoped, and audited: each successful read appends a get_skill row to the access log. Raises 22023 for an unknown or hidden skill.';
COMMENT ON FUNCTION pgokf.get_script(bigint, text) IS
    'Return one package script as pgokf.script_result: language, package-relative path and owning skill, size, SHA-256, declared runtime/arguments/exit codes when known, and the exact stored bytes (never body_text). Reader-level, tenant-scoped, and audited: each successful read appends a get_script row to the access log. Raises 22023 for an unknown or hidden script.';
COMMENT ON FUNCTION pgokf.get_reference(bigint, text, boolean) IS
    'Return one package reference or asset as pgokf.reference_result: format and media type, package-relative path and owning skill, size, SHA-256, its text when textual, and the exact stored bytes when include_bytes (the default) is true. Reader-level, tenant-scoped, and audited: each successful read appends a get_reference row to the access log (detail metadata when the bytes were not requested). Raises 22023 for an unknown or hidden reference.';

-- The web identity tables' appended column order. The existing rows (people
-- signed in by password and by an identity provider, and their sessions) are
-- carried across: the appended columns are dropped and re-added at the end
-- with their checks, the values restored by key, then the cross-column checks
-- re-added.
CREATE TEMPORARY TABLE live_drift_users AS SELECT name, display_name, provider FROM pgokf_web.users;
CREATE TEMPORARY TABLE live_drift_sessions AS SELECT nonce, provider FROM pgokf_web.sessions;
ALTER TABLE pgokf_web.users DROP COLUMN display_name, DROP COLUMN provider;
ALTER TABLE pgokf_web.users
    ADD COLUMN display_name text
        CONSTRAINT users_display_name_check
        CHECK (display_name ~ '^[^[:cntrl:]]+$' AND char_length(display_name) <= 256),
    ADD COLUMN provider text
        CONSTRAINT users_provider_check CHECK (provider ~ '^[a-z0-9][a-z0-9-]{0,31}$');
UPDATE pgokf_web.users AS u SET display_name = d.display_name, provider = d.provider
FROM live_drift_users AS d WHERE d.name = u.name;
ALTER TABLE pgokf_web.users
    ADD CONSTRAINT users_sign_in_check CHECK ((password_hash IS NULL) <> (provider IS NULL));
COMMENT ON COLUMN pgokf_web.users.display_name IS
    'What the person is called wherever the UI shows them: for a person an identity provider signed in, the name the provider reports, refreshed at every sign-in; for a password person, what an admin entered, or NULL to show the sign-in name. Their OKF actor stays human:<name>.';
COMMENT ON COLUMN pgokf_web.users.provider IS
    'For a person without a password, the identity provider that signed them in (identity_providers.id); NULL for a password person. With users_sign_in_check this makes a name belong to exactly one way in: a provider never signs in a password person''s name, nor a name another provider brought.';
ALTER TABLE pgokf_web.sessions DROP COLUMN provider;
ALTER TABLE pgokf_web.sessions
    ADD COLUMN provider text
        CONSTRAINT sessions_provider_check CHECK (provider ~ '^[a-z0-9][a-z0-9-]{0,31}$');
UPDATE pgokf_web.sessions AS s SET provider = d.provider
FROM live_drift_sessions AS d WHERE d.nonce = s.nonce;
ALTER TABLE pgokf_web.sessions
    ADD CONSTRAINT sessions_provider_mode_check CHECK (provider IS NULL OR mode = 'oidc');
COMMENT ON COLUMN pgokf_web.sessions.provider IS 'For a session opened by an identity provider set up on the Admin page: that provider (identity_providers.id), so switching it off or removing it ends its sessions alone; NULL for a password session, or one the oidc mode''s own provider opened.';
DROP TABLE live_drift_users, live_drift_sessions;

-- Superseded web identity comments (from before identity providers could be
-- many).
COMMENT ON SCHEMA pgokf_web IS
    'The web UI''s identity state: the people a local sign-in knows, the sessions the UI has issued, the bearer tokens the MCP server accepts over HTTP, and the identity provider an admin set up. Owned by the extension so it is transactional, shared by every UI instance, and dumped with the catalog; read and written by pgokf_writer only, and never by the extension itself.';
COMMENT ON TABLE pgokf_web.users IS
    'People the web UI''s users identity mode signs in: one row per person with their role on the viewer < uploader < editor < approver < admin ladder and an Argon2id hash of their password. Managed by pgokf-web (its user add / set-password commands and the Admin page); pgokf_writer only, so a reader never sees a hash.';
COMMENT ON COLUMN pgokf_web.users.name IS
    'The sign-in name: one plain token of letters, digits, and . _ @ + - (at most 128), which is also the person''s OKF actor (human:<name>).';
COMMENT ON COLUMN pgokf_web.users.role IS
    'The role the UI grants on every request: viewer, uploader, editor, approver, or admin (each holds everything below it). A session cookie carries no role, so a change here takes effect at once.';
COMMENT ON COLUMN pgokf_web.users.password_hash IS
    'An Argon2id PHC string of the password, or NULL for a person the identity provider signed in: they have no password here, a password sign-in under their name is refused, and their row exists so an admin can see them and set their role. A fingerprint of the hash is bound into every session a password opens, so a changed password ends the sessions opened before it.';
COMMENT ON COLUMN pgokf_web.users.created_at IS 'When the person was added.';
COMMENT ON COLUMN pgokf_web.users.updated_at IS 'When the role or password last changed.';
