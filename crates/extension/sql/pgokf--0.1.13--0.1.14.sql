-- pgokf extension upgrade: 0.1.13 -> 0.1.14
--
-- 0.1.14 makes a logical backup of the catalog complete. Before it, pg_dump
-- emitted only the CREATE EXTENSION statement for pgokf: the contents of the
-- extension-owned tables were skipped (PostgreSQL dumps an extension's tables
-- only when the extension registers them with pg_extension_config_dump), so a
-- restore came back as an empty catalog. This script:
--
--   * installs pgokf_private.register_dump_relations() and calls it: every
--     pgokf.* and pgokf_private.* table and sequence becomes an extension
--     configuration relation, discovered from the extension's own dependency
--     graph so nothing is listed by hand and nothing is missed (future upgrade
--     scripts that add a relation call it again as their last statement);
--   * installs pgokf_private.config_restore_upsert, a BEFORE INSERT trigger on
--     the singleton policy table: a restore replays CREATE EXTENSION (which
--     seeds the default row) and then COPYs the dumped row into the same key,
--     and the trigger folds that COPY into an UPDATE of the seeded row so the
--     dumped policy is carried across instead of failing on a duplicate key.
--   * installs pgokf.bm25_hits, the SECURITY DEFINER helper that now runs the
--     ParadeDB pg_search BM25 hit query behind concept_search when
--     search_backend = bm25. For any session that is not the table owner,
--     row-level security wraps pgokf.concepts in a security-barrier subquery
--     that pg_search cannot plan its custom scan over, so before 0.1.14 the
--     bm25 backend raised "Unsupported query shape" for every non-superuser
--     reader. The helper runs with the owner's privileges and applies the same
--     explicit pgokf.tenant predicate the policies enforce.
--
-- No table, type, function signature, index, grant, or configuration key of
-- the 0.1.13 install is dropped, renamed, or rewritten, and no existing row is
-- touched: the trigger only fires on an INSERT into a table that receives
-- exactly one, at CREATE EXTENSION time, or during a restore. A catalog
-- upgraded with this script is identical to a fresh 0.1.14 install.
--
-- Never DROP, TRUNCATE, DELETE, or rewrite existing catalog data in an upgrade
-- script: doing so would break the no-data-loss guarantee asserted by the
-- api_stability upgrade tests.

CREATE FUNCTION pgokf_private.config_restore_upsert() RETURNS trigger
    LANGUAGE plpgsql
    SET search_path = pg_catalog, pg_temp
    AS $fn$
DECLARE
    assignments text;
BEGIN
    IF NOT EXISTS (SELECT 1 FROM pgokf_private.config WHERE singleton) THEN
        RETURN NEW;
    END IF;
    SELECT string_agg(format('%I = ($1).%I', attname, attname), ', ' ORDER BY attnum)
      INTO assignments
      FROM pg_catalog.pg_attribute
     WHERE attrelid = 'pgokf_private.config'::regclass
       AND attnum > 0
       AND NOT attisdropped
       AND attname <> 'singleton';
    EXECUTE format('UPDATE pgokf_private.config SET %s WHERE singleton', assignments)
      USING NEW;
    RETURN NULL;
END
$fn$;

CREATE TRIGGER config_restore_upsert
    BEFORE INSERT ON pgokf_private.config
    FOR EACH ROW EXECUTE FUNCTION pgokf_private.config_restore_upsert();
-- ALWAYS: a logical-replication subscriber (session_replication_role = replica)
-- applying the row must fold it too. pg_restore --disable-triggers is the one
-- path that bypasses it, and is documented as unsupported for this table.
ALTER TABLE pgokf_private.config ENABLE ALWAYS TRIGGER config_restore_upsert;

REVOKE ALL ON FUNCTION pgokf_private.config_restore_upsert() FROM PUBLIC;
COMMENT ON FUNCTION pgokf_private.config_restore_upsert() IS
    'BEFORE INSERT trigger on pgokf_private.config: when the singleton policy row already exists (seeded by CREATE EXTENSION), an incoming row - the COPY of a pg_dump archive during restore - replaces it column for column (a column omitted by the INSERT takes its default) and the insert is suppressed, so a restore carries the dumped policy across without a duplicate-key failure. Policy changes go through pgokf.set_config, never through INSERT.';



-- Register every extension-owned table and sequence with pg_dump, so a logical
-- backup carries the catalog's rows and sequence positions rather than just the
-- CREATE EXTENSION statement. Discovered from the extension's own dependency
-- graph, so nothing is listed by hand. pg_extension_config_dump may only run
-- from an extension script: the fresh install calls this at the end of its
-- SQL, and EVERY upgrade script that creates a table or sequence must call it
-- again as its last statement.
CREATE FUNCTION pgokf_private.register_dump_relations() RETURNS void
    LANGUAGE plpgsql
    SET search_path = pg_catalog, pg_temp
    AS $fn$
DECLARE
    rel regclass;
BEGIN
    FOR rel IN
        SELECT c.oid::regclass
          FROM pg_catalog.pg_depend d
          JOIN pg_catalog.pg_extension e ON e.oid = d.refobjid AND e.extname = 'pgokf'
          JOIN pg_catalog.pg_class c ON c.oid = d.objid
         WHERE d.classid = 'pg_catalog.pg_class'::regclass
           AND d.refclassid = 'pg_catalog.pg_extension'::regclass
           AND d.deptype = 'e'
           AND c.relkind IN ('r', 'S')
         ORDER BY c.oid
    LOOP
        PERFORM pg_catalog.pg_extension_config_dump(rel, '');
    END LOOP;
END
$fn$;

REVOKE ALL ON FUNCTION pgokf_private.register_dump_relations() FROM PUBLIC;
COMMENT ON FUNCTION pgokf_private.register_dump_relations() IS
    'Internal, extension-script only: registers every pgokf-owned table and sequence with pg_extension_config_dump so pg_dump carries the catalog. Called at the end of the fresh install and of every upgrade script that adds a relation.';

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
    'Internal helper behind concept_search when search_backend = bm25; not part of the stable API. Runs the ParadeDB pg_search BM25 hit query with the owner''s privileges (row-level security wraps the catalog tables in a shape pg_search cannot plan for non-owners) while applying the same pgokf.tenant scoping the policies enforce, over active bundles only, with concept_search''s filters, keyset cursor, and limit. Reader-level; returns exactly the rows concept_search would.';

-- Last, so the relations this and any future script create are all covered.
SELECT pgokf_private.register_dump_relations();
