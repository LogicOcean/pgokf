-- pgokf extension upgrade: 0.1.1 -> 0.1.2
--
-- Additive, forward-compatible upgrade for the opt-in "raw source storage"
-- feature. It brings an existing 0.1.1 install fully to 0.1.2 with NO data
-- loss: no table, type, function, index, grant, or comment defined by the
-- prior install is dropped, renamed, or rewritten. It only ADDS:
--
--   1. the `store_source` policy column on pgokf_private.config (default false,
--      so behavior is unchanged until an admin opts in);
--   2. the pgokf.concept_source table (+ TOAST compression, grants, comments);
--   3. the pgokf.get_concept_source(bigint, text) reader function;
--   4. the pgokf.export_sources(bigint, text) admin function;
--
-- with the same grants, hardening, and comments the 0.1.2 full install ships.
-- `ALTER EXTENSION pgokf UPDATE TO '0.1.2'` runs this in a single transaction.
--
-- Never DROP, TRUNCATE, DELETE, or rewrite existing catalog data in an upgrade
-- script: doing so would break the no-data-loss guarantee.

-- 1. Durable config: the store_source toggle. A constant DEFAULT means no table
--    rewrite on modern PostgreSQL, and the default preserves current behavior.
ALTER TABLE pgokf_private.config
    ADD COLUMN IF NOT EXISTS store_source boolean NOT NULL DEFAULT false;

COMMENT ON COLUMN pgokf_private.config.store_source IS
    'Whether sync stores each concept''s verbatim source bytes in pgokf.concept_source (true = small self-contained tier: the original files live in Postgres) or leaves the source in its external object-store/data-lake (false, the default = enterprise tier: Postgres holds only metadata and search). Not retroactive: a change takes effect for bundles synced or refreshed afterward; existing rows keep their stored source (or absence of one) until refresh_bundle re-indexes them.';

-- 2. The opt-in verbatim source-byte store, cascading from pgokf.concepts.
CREATE TABLE IF NOT EXISTS pgokf.concept_source (
    bundle_id   bigint  NOT NULL,
    concept_id  text    NOT NULL,
    raw_content bytea   NOT NULL,
    byte_size   integer NOT NULL,
    CONSTRAINT concept_source_pkey PRIMARY KEY (bundle_id, concept_id),
    CONSTRAINT concept_source_concept_fk
        FOREIGN KEY (bundle_id, concept_id)
        REFERENCES pgokf.concepts (bundle_id, id)
        ON DELETE CASCADE
);

-- Prefer lz4 compression for the source bytes when this PostgreSQL build ships
-- it; fall back to the default pglz otherwise (an lz4-less build must not abort
-- the upgrade). The exception handler establishes a subtransaction, so a failed
-- ALTER rolls back to it and the upgrade continues.
DO $pgokf_lz4$
BEGIN
    ALTER TABLE pgokf.concept_source
        ALTER COLUMN raw_content SET COMPRESSION lz4;
EXCEPTION WHEN OTHERS THEN
    NULL;
END
$pgokf_lz4$;

COMMENT ON TABLE pgokf.concept_source IS
    'Opt-in verbatim source bytes of each concept file, populated only when the store_source configuration key is enabled (small self-contained tier). Rows cascade from pgokf.concepts, so removing a concept or unregistering a bundle drops the stored source automatically.';
COMMENT ON COLUMN pgokf.concept_source.raw_content IS
    'The exact, unmodified bytes of the concept source file, as read at sync time; hashes to pgokf.concepts.file_hash (BLAKE3).';
COMMENT ON COLUMN pgokf.concept_source.byte_size IS
    'Length in bytes of raw_content, recorded so a reader can size a retrieval without detoasting the content.';

GRANT SELECT ON pgokf.concept_source TO pgokf_reader;

-- 3. Reader-level retrieval: return the exact stored bytes to the client.
CREATE OR REPLACE FUNCTION pgokf."get_concept_source"(
    "bundle_id" bigint,
    "concept_id" TEXT
) RETURNS bytea
STRICT STABLE
LANGUAGE c
AS 'MODULE_PATHNAME', 'get_concept_source_wrapper';

ALTER FUNCTION pgokf.get_concept_source(bigint, text)
    SET search_path = pg_catalog, pg_temp;
REVOKE ALL ON FUNCTION pgokf.get_concept_source(bigint, text) FROM PUBLIC;
GRANT EXECUTE ON FUNCTION pgokf.get_concept_source(bigint, text) TO pgokf_reader;
COMMENT ON FUNCTION pgokf.get_concept_source(bigint, text) IS
    'Return the verbatim stored source bytes of one concept as bytea. Reader-level (same disclosure as body_text); raises 22023 when the concept exists but no source was stored, or when no such concept exists.';

-- 4. Admin-level reconstruction of the bundle on disk, byte-for-byte.
CREATE OR REPLACE FUNCTION pgokf."export_sources"(
    "bundle_id" bigint,
    "dest_dir" TEXT
) RETURNS pgokf.export_result
STRICT
LANGUAGE c
AS 'MODULE_PATHNAME', 'export_sources_wrapper';

ALTER FUNCTION pgokf.export_sources(bigint, text)
    SECURITY DEFINER SET search_path = pg_catalog, pg_temp;
REVOKE ALL ON FUNCTION pgokf.export_sources(bigint, text) FROM PUBLIC;
GRANT EXECUTE ON FUNCTION pgokf.export_sources(bigint, text) TO pgokf_admin;
COMMENT ON FUNCTION pgokf.export_sources(bigint, text) IS
    'Reconstruct a bundle''s stored source files under dest_dir, recreating the bundle-relative tree and verifying each file against its BLAKE3 file_hash; returns pgokf.export_result (concepts_rows = files written, bytes_written = total bytes). Admin-only; dest_dir must be an existing, writable, canonical directory contained within pgokf.allowed_roots when configured. Raises 22023 (bad bundle/dir), 42501 (dir not writable), or XX000 (a stored source fails its hash check, verified before any write).';
