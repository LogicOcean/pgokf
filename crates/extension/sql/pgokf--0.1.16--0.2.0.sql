-- SPDX-License-Identifier: AGPL-3.0-only
-- pgokf extension upgrade: 0.1.16 -> 0.2.0
--
-- 0.2.0 stores agent plugin content in the catalog. A bundle may carry Agent
-- Skills packages (a directory with a SKILL.md and optional scripts/,
-- references/, and assets/); the sync now discovers those files, projects
-- the manifest as a virtual type: Skill concept and every resource as a
-- virtual Script or Reference concept, and keeps their EXACT bytes in three
-- new typed tables so a workspace plugin can be rebuilt byte for byte
-- (specification 5.3, 15-18, 21):
--
--   * pgokf.skills - the manifest's original bytes, complete frontmatter,
--     package directory, and package hash;
--   * pgokf.scripts - a script's exact UTF-8 bytes, language, interpreter,
--     size, and SHA-256;
--   * pgokf.reference_documents - a reference or asset's exact bytes, format,
--     media type, size, SHA-256, and text when textual.
--
-- Three reader-level, tenant-scoped, audited retrieval functions return those
-- bytes (pgokf.get_skill, pgokf.get_script, pgokf.get_reference) with three
-- composite result types, and the access log accepts their operation names.
-- Package membership is projected into pgokf.links as edges whose
-- link_kind is 'package' and whose link_relation is USES (skill -> script) or
-- REFERENCES (skill -> reference/asset); the link_kind and link_relation
-- comments are refreshed to say so. The discovery, staging, and projection
-- logic lives in the 0.2.0 shared library; this script creates the SQL
-- objects it reads and writes.
--
-- Every statement is additive: no row is touched, no data-bearing object is
-- dropped. The one DROP is of the access log's op CHECK constraint, which is
-- immediately re-created with the three new operations; a constraint carries
-- no data. An existing bundle gains its packages on its next refresh: files
-- below scripts/, references/, and assets/ of a SKILL.md directory were not
-- discovered before and now are. A catalog upgraded with this script is
-- identical to a fresh 0.2.0 install.
--
-- Never DROP, TRUNCATE, DELETE, or rewrite existing catalog data in an upgrade
-- script: doing so would break the no-data-loss guarantee asserted by the
-- api_stability upgrade tests.

-- ===========================================================================
-- 1. The typed projections (the package_tables block of
--    src/catalog/packages.rs, verbatim).
CREATE TABLE pgokf.skills (
    bundle_id        bigint NOT NULL,
    concept_id       text   NOT NULL,
    visibility       text   NOT NULL CHECK (visibility IN ('public', 'internal', 'private')),
    agent_skill      jsonb  NOT NULL,
    skill_md         bytea  NOT NULL,
    package_root     text   NOT NULL,
    package_hash     text   NOT NULL,
    source_file_hash text   NOT NULL,
    tenant_id        text   NOT NULL DEFAULT 'default',
    CONSTRAINT skills_pkey PRIMARY KEY (bundle_id, concept_id),
    CONSTRAINT skills_concept_fk
        FOREIGN KEY (bundle_id, concept_id)
        REFERENCES pgokf.concepts (bundle_id, id)
        ON DELETE CASCADE
);

CREATE TABLE pgokf.scripts (
    bundle_id          bigint NOT NULL,
    concept_id         text   NOT NULL,
    language           text   NOT NULL,
    visibility         text   NOT NULL CHECK (visibility IN ('public', 'internal', 'private')),
    author             jsonb,
    origin             jsonb,
    license            text,
    runtime            jsonb,
    arguments          jsonb,
    exit_codes         jsonb,
    exact_bytes        bytea  NOT NULL,
    byte_size          bigint NOT NULL,
    executable_sha256  text   NOT NULL,
    source_path        text   NOT NULL,
    package_concept_id text,
    source_file_hash   text   NOT NULL,
    script_tsv         tsvector,
    tenant_id          text   NOT NULL DEFAULT 'default',
    CONSTRAINT scripts_pkey PRIMARY KEY (bundle_id, concept_id),
    CONSTRAINT scripts_concept_fk
        FOREIGN KEY (bundle_id, concept_id)
        REFERENCES pgokf.concepts (bundle_id, id)
        ON DELETE CASCADE
);

CREATE INDEX scripts_package_idx ON pgokf.scripts (bundle_id, package_concept_id);

CREATE TABLE pgokf.reference_documents (
    bundle_id          bigint NOT NULL,
    concept_id         text   NOT NULL,
    visibility         text   NOT NULL CHECK (visibility IN ('public', 'internal', 'private')),
    format             text   NOT NULL,
    media_type         text   NOT NULL,
    author             jsonb,
    origin             jsonb,
    license            text,
    exact_bytes        bytea  NOT NULL,
    byte_size          bigint NOT NULL,
    content_sha256     text   NOT NULL,
    text_body          text,
    extracted_text     text,
    extraction         jsonb,
    source_path        text   NOT NULL,
    package_concept_id text,
    source_file_hash   text   NOT NULL,
    reference_tsv      tsvector,
    tenant_id          text   NOT NULL DEFAULT 'default',
    CONSTRAINT reference_documents_pkey PRIMARY KEY (bundle_id, concept_id),
    CONSTRAINT reference_documents_concept_fk
        FOREIGN KEY (bundle_id, concept_id)
        REFERENCES pgokf.concepts (bundle_id, id)
        ON DELETE CASCADE
);

CREATE INDEX reference_documents_package_idx
    ON pgokf.reference_documents (bundle_id, package_concept_id);

-- Multi-tenant isolation (see pgokf.bundles): opt-in-by-usage RLS on the
-- denormalized tenant_id. Not forced, so the SECURITY DEFINER sync path bypasses
-- it to project a single-tenant bundle's packages.
ALTER TABLE pgokf.skills ENABLE ROW LEVEL SECURITY;
CREATE POLICY skills_tenant_isolation ON pgokf.skills
    USING (((pg_catalog.current_setting('pgokf.tenant', true) IS NULL
             OR pg_catalog.current_setting('pgokf.tenant', true) = '')
            AND NOT (SELECT pgokf.tenant_required()))
        OR tenant_id = pg_catalog.current_setting('pgokf.tenant', true))
    WITH CHECK (((pg_catalog.current_setting('pgokf.tenant', true) IS NULL
                  OR pg_catalog.current_setting('pgokf.tenant', true) = '')
                 AND NOT (SELECT pgokf.tenant_required()))
        OR tenant_id = pg_catalog.current_setting('pgokf.tenant', true));
ALTER TABLE pgokf.scripts ENABLE ROW LEVEL SECURITY;
CREATE POLICY scripts_tenant_isolation ON pgokf.scripts
    USING (((pg_catalog.current_setting('pgokf.tenant', true) IS NULL
             OR pg_catalog.current_setting('pgokf.tenant', true) = '')
            AND NOT (SELECT pgokf.tenant_required()))
        OR tenant_id = pg_catalog.current_setting('pgokf.tenant', true))
    WITH CHECK (((pg_catalog.current_setting('pgokf.tenant', true) IS NULL
                  OR pg_catalog.current_setting('pgokf.tenant', true) = '')
                 AND NOT (SELECT pgokf.tenant_required()))
        OR tenant_id = pg_catalog.current_setting('pgokf.tenant', true));
ALTER TABLE pgokf.reference_documents ENABLE ROW LEVEL SECURITY;
CREATE POLICY reference_documents_tenant_isolation ON pgokf.reference_documents
    USING (((pg_catalog.current_setting('pgokf.tenant', true) IS NULL
             OR pg_catalog.current_setting('pgokf.tenant', true) = '')
            AND NOT (SELECT pgokf.tenant_required()))
        OR tenant_id = pg_catalog.current_setting('pgokf.tenant', true))
    WITH CHECK (((pg_catalog.current_setting('pgokf.tenant', true) IS NULL
                  OR pg_catalog.current_setting('pgokf.tenant', true) = '')
                 AND NOT (SELECT pgokf.tenant_required()))
        OR tenant_id = pg_catalog.current_setting('pgokf.tenant', true));

-- Prefer lz4 for the exact-byte columns when this build ships it (see
-- pgokf.concept_source for the rationale); silently keep pglz otherwise.
DO $pgokf_lz4$
BEGIN
    ALTER TABLE pgokf.skills ALTER COLUMN skill_md SET COMPRESSION lz4;
    ALTER TABLE pgokf.scripts ALTER COLUMN exact_bytes SET COMPRESSION lz4;
    ALTER TABLE pgokf.reference_documents ALTER COLUMN exact_bytes SET COMPRESSION lz4;
EXCEPTION WHEN OTHERS THEN
    NULL;
END
$pgokf_lz4$;

COMMENT ON TABLE pgokf.skills IS
    'Exact projection of every Agent Skills package manifest (SKILL.md) the sync discovered: the original bytes, the complete parsed frontmatter, the package directory, and the package hash over every owned resource. One row per type: Skill concept; rows cascade from pgokf.concepts. Kept whether or not store_source is on, so a plugin can always be rebuilt byte for byte.';
COMMENT ON COLUMN pgokf.skills.visibility IS
    'public, internal, or private: the frontmatter''s visibility when it declares one, otherwise internal. Inherited by the package''s scripts, references, and assets.';
COMMENT ON COLUMN pgokf.skills.agent_skill IS
    'The complete original SKILL.md frontmatter as parsed (name, description, license, compatibility, metadata, allowed-tools, and any unknown field), never rewritten.';
COMMENT ON COLUMN pgokf.skills.skill_md IS
    'The exact, unmodified bytes of SKILL.md as read at sync time; hashes to source_file_hash (BLAKE3).';
COMMENT ON COLUMN pgokf.skills.package_root IS
    'Bundle-relative directory of the package (the manifest''s directory); empty when the bundle root itself is the package.';
COMMENT ON COLUMN pgokf.skills.package_hash IS
    'BLAKE3 over a domain-separated encoding of the classifier version, the manifest''s file hash, and every owned member as class, package-relative path, and byte hash (sorted by path): changes whenever any member is added, removed, or edited, even when SKILL.md is unchanged.';
COMMENT ON COLUMN pgokf.skills.source_file_hash IS
    'The manifest''s BLAKE3 file hash; equals pgokf.concepts.file_hash for the same concept.';
COMMENT ON COLUMN pgokf.skills.tenant_id IS
    'Multi-tenant owner, denormalized from the bundle for a local row-level-security predicate; always equals the bundle''s tenant_id.';

COMMENT ON TABLE pgokf.scripts IS
    'Exact projection of every executable helper found below a skill package''s scripts/ directory (a virtual type: Script concept): the exact UTF-8 bytes, size, SHA-256, inferred language and interpreter, and the owning package. Retrieval through pgokf.get_script returns these bytes, never body_text. Rows cascade from pgokf.concepts. Binary files under scripts/ are never scripts.';
COMMENT ON COLUMN pgokf.scripts.language IS
    'Canonical language identifier (bash, shell, python, javascript, ...) inferred from the shebang first and the file extension second, or unknown.';
COMMENT ON COLUMN pgokf.scripts.visibility IS
    'public, internal, or private; inherited from the owning skill.';
COMMENT ON COLUMN pgokf.scripts.author IS
    'Author declaration when package metadata supplies one; NULL for a discovered helper.';
COMMENT ON COLUMN pgokf.scripts.origin IS
    'Origin (repository, revision, path) when package metadata supplies one; NULL for a discovered helper.';
COMMENT ON COLUMN pgokf.scripts.license IS
    'License identifier when package metadata supplies one.';
COMMENT ON COLUMN pgokf.scripts.runtime IS
    'The interpreter the script''s shebang names, as a JSON object whose executable key holds the shebang command, or NULL when it has no shebang. Descriptive only; never an execution authorization.';
COMMENT ON COLUMN pgokf.scripts.arguments IS
    'Declared argument, flag, and environment inputs when package metadata supplies them; NULL means unspecified.';
COMMENT ON COLUMN pgokf.scripts.exit_codes IS
    'Documented exit statuses when package metadata supplies them.';
COMMENT ON COLUMN pgokf.scripts.exact_bytes IS
    'The exact, unmodified script bytes as read at sync time (valid UTF-8); the authoritative retrieval payload.';
COMMENT ON COLUMN pgokf.scripts.byte_size IS
    'Length in bytes of exact_bytes, recorded so a reader can size a retrieval without detoasting the content.';
COMMENT ON COLUMN pgokf.scripts.executable_sha256 IS
    'Lowercase hexadecimal SHA-256 of exact_bytes, the identity a workspace lockfile records.';
COMMENT ON COLUMN pgokf.scripts.source_path IS
    'Path of the file relative to its package root (scripts/check.sh); with the package root this is the bundle-relative path and the concept ID.';
COMMENT ON COLUMN pgokf.scripts.package_concept_id IS
    'Concept ID of the owning skill (its SKILL.md without .md); NULL only for a standalone script, which this release does not ingest.';
COMMENT ON COLUMN pgokf.scripts.source_file_hash IS
    'The script''s BLAKE3 file hash; equals pgokf.concepts.file_hash for the same concept.';
COMMENT ON COLUMN pgokf.scripts.script_tsv IS
    'Weighted tsvector over the script text for type-specific search, built with the bundle''s text-search configuration at sync time.';
COMMENT ON COLUMN pgokf.scripts.tenant_id IS
    'Multi-tenant owner, denormalized from the bundle for a local row-level-security predicate; always equals the bundle''s tenant_id.';

COMMENT ON TABLE pgokf.reference_documents IS
    'Exact projection of every file found below a skill package''s references/ or assets/ directory (a virtual type: Reference concept): the exact bytes, size, SHA-256, format and media type, its text when it is textual, and the owning package. Retrieval through pgokf.get_reference returns these bytes. Rows cascade from pgokf.concepts. Named reference_documents because references is a reserved word.';
COMMENT ON COLUMN pgokf.reference_documents.visibility IS
    'public, internal, or private; inherited from the owning skill.';
COMMENT ON COLUMN pgokf.reference_documents.format IS
    'Canonical lower-case format (markdown, text, json, yaml, csv, toml, html, xml, pdf, binary) or the media type for images and other typed binaries.';
COMMENT ON COLUMN pgokf.reference_documents.media_type IS
    'IANA media type inferred from verified magic bytes first and the file extension second (text/markdown, image/png, application/octet-stream, ...).';
COMMENT ON COLUMN pgokf.reference_documents.author IS
    'Author declaration when package metadata supplies one; NULL for a discovered file.';
COMMENT ON COLUMN pgokf.reference_documents.origin IS
    'Origin (repository, revision, path) when package metadata supplies one; NULL for a discovered file.';
COMMENT ON COLUMN pgokf.reference_documents.license IS
    'License identifier when package metadata supplies one.';
COMMENT ON COLUMN pgokf.reference_documents.exact_bytes IS
    'The exact, unmodified bytes as read at sync time, text or binary; the authoritative retrieval payload.';
COMMENT ON COLUMN pgokf.reference_documents.byte_size IS
    'Length in bytes of exact_bytes, recorded so a reader can size a retrieval without detoasting the content.';
COMMENT ON COLUMN pgokf.reference_documents.content_sha256 IS
    'Lowercase hexadecimal SHA-256 of exact_bytes, the identity a workspace lockfile records.';
COMMENT ON COLUMN pgokf.reference_documents.text_body IS
    'The bytes decoded as UTF-8 when the file is textual (its format is not binary); NULL for a binary asset. Frontmatter, if any, is part of the text: a package reference is stored verbatim.';
COMMENT ON COLUMN pgokf.reference_documents.extracted_text IS
    'Text extracted from a binary reference by an optional, bounded extractor; NULL until one runs. Never replaces exact_bytes.';
COMMENT ON COLUMN pgokf.reference_documents.extraction IS
    'Which extractor and version produced extracted_text; NULL until one runs.';
COMMENT ON COLUMN pgokf.reference_documents.source_path IS
    'Path of the file relative to its package root (references/guide.md or assets/topology.png); the first segment records whether it is a reference or an asset.';
COMMENT ON COLUMN pgokf.reference_documents.package_concept_id IS
    'Concept ID of the owning skill (its SKILL.md without .md); NULL only for a standalone typed Reference, which this release does not project.';
COMMENT ON COLUMN pgokf.reference_documents.source_file_hash IS
    'The file''s BLAKE3 hash; equals pgokf.concepts.file_hash for the same concept.';
COMMENT ON COLUMN pgokf.reference_documents.reference_tsv IS
    'Weighted tsvector over text_body for type-specific search, built with the bundle''s text-search configuration at sync time; NULL for a binary asset.';
COMMENT ON COLUMN pgokf.reference_documents.tenant_id IS
    'Multi-tenant owner, denormalized from the bundle for a local row-level-security predicate; always equals the bundle''s tenant_id.';

GRANT SELECT ON pgokf.skills TO pgokf_reader;
GRANT SELECT ON pgokf.scripts TO pgokf_reader;
GRANT SELECT ON pgokf.reference_documents TO pgokf_reader;

-- ===========================================================================
-- 2. The composite result types (the package_result_types block, verbatim).
CREATE TYPE pgokf.skill_result AS (
    bundle_id    bigint,
    concept_id   text,
    name         text,
    description  text,
    package_root text,
    package_hash text,
    file_hash    text,
    visibility   text,
    agent_skill  jsonb,
    skill_md     bytea,
    resources    jsonb
);

COMMENT ON TYPE pgokf.skill_result IS
    'One Agent Skills package from pgokf.get_skill: its identity, name and description, package directory and hash, visibility, the complete original frontmatter, the exact SKILL.md bytes, and a JSON array of the resources it owns (concept_id, class script/reference/asset, package-relative path, byte_size, sha256, file_hash, and language or media_type) ordered by path.';

CREATE TYPE pgokf.script_result AS (
    bundle_id          bigint,
    concept_id         text,
    title              text,
    language           text,
    source_path        text,
    package_concept_id text,
    byte_size          bigint,
    executable_sha256  text,
    runtime            jsonb,
    arguments          jsonb,
    exit_codes         jsonb,
    exact_bytes        bytea
);

COMMENT ON TYPE pgokf.script_result IS
    'One package script from pgokf.get_script: its identity, title, language, package-relative path and owning skill, size and SHA-256, the declared runtime/arguments/exit codes when known, and the exact stored bytes.';

CREATE TYPE pgokf.reference_result AS (
    bundle_id          bigint,
    concept_id         text,
    title              text,
    format             text,
    media_type         text,
    source_path        text,
    package_concept_id text,
    byte_size          bigint,
    content_sha256     text,
    text_body          text,
    exact_bytes        bytea
);

COMMENT ON TYPE pgokf.reference_result IS
    'One package reference or asset from pgokf.get_reference: its identity, title, format and media type, package-relative path and owning skill, size and SHA-256, its text when textual, and the exact stored bytes (NULL when include_bytes is false).';

-- ===========================================================================
-- 3. The retrieval functions, declared exactly as the 0.2.0 install script
--    declares them (STRICT STABLE, C-language wrappers exported by the 0.2.0
--    shared library), then hardened as the package_function_hardening block
--    hardens them.
CREATE FUNCTION pgokf."get_skill"(
    "bundle_id" bigint,
    "concept_id" TEXT
) RETURNS pgokf.skill_result
STRICT STABLE
LANGUAGE c
AS 'MODULE_PATHNAME', 'get_skill_wrapper';

CREATE FUNCTION pgokf."get_script"(
    "bundle_id" bigint,
    "concept_id" TEXT
) RETURNS pgokf.script_result
STRICT STABLE
LANGUAGE c
AS 'MODULE_PATHNAME', 'get_script_wrapper';

CREATE FUNCTION pgokf."get_reference"(
    "bundle_id" bigint,
    "concept_id" TEXT,
    "include_bytes" bool DEFAULT true
) RETURNS pgokf.reference_result
STRICT STABLE
LANGUAGE c
AS 'MODULE_PATHNAME', 'get_reference_wrapper';
ALTER FUNCTION pgokf.get_skill(bigint, text)
    SECURITY DEFINER SET search_path = pg_catalog, pg_temp;
REVOKE ALL ON FUNCTION pgokf.get_skill(bigint, text) FROM PUBLIC;
GRANT EXECUTE ON FUNCTION pgokf.get_skill(bigint, text) TO pgokf_reader;
COMMENT ON FUNCTION pgokf.get_skill(bigint, text) IS
    'Return one Agent Skills package as pgokf.skill_result: name, description, package directory and hash, visibility, the complete original frontmatter, the exact SKILL.md bytes, and the JSON listing of its scripts, references, and assets. Reader-level, tenant-scoped, and audited: each successful read appends a get_skill row to the access log. Raises 22023 for an unknown skill.';

ALTER FUNCTION pgokf.get_script(bigint, text)
    SECURITY DEFINER SET search_path = pg_catalog, pg_temp;
REVOKE ALL ON FUNCTION pgokf.get_script(bigint, text) FROM PUBLIC;
GRANT EXECUTE ON FUNCTION pgokf.get_script(bigint, text) TO pgokf_reader;
COMMENT ON FUNCTION pgokf.get_script(bigint, text) IS
    'Return one package script as pgokf.script_result: language, package-relative path and owning skill, size, SHA-256, declared runtime/arguments/exit codes when known, and the exact stored bytes (never body_text). Reader-level, tenant-scoped, and audited: each successful read appends a get_script row to the access log. Raises 22023 for an unknown script.';

ALTER FUNCTION pgokf.get_reference(bigint, text, boolean)
    SECURITY DEFINER SET search_path = pg_catalog, pg_temp;
REVOKE ALL ON FUNCTION pgokf.get_reference(bigint, text, boolean) FROM PUBLIC;
GRANT EXECUTE ON FUNCTION pgokf.get_reference(bigint, text, boolean) TO pgokf_reader;
COMMENT ON FUNCTION pgokf.get_reference(bigint, text, boolean) IS
    'Return one package reference or asset as pgokf.reference_result: format and media type, package-relative path and owning skill, size, SHA-256, its text when textual, and the exact stored bytes when include_bytes (the default) is true. Reader-level, tenant-scoped, and audited: each successful read appends a get_reference row to the access log (detail metadata when the bytes were not requested). Raises 22023 for an unknown reference.';

-- ===========================================================================
-- 4. The access log accepts the three new audited operations. The 0.1.8
--    script created the op CHECK inline (PostgreSQL named it
--    access_log_op_check); the fresh 0.2.0 table declares it under that same
--    name with the extended list.
ALTER TABLE pgokf_private.access_log DROP CONSTRAINT IF EXISTS access_log_op_check;
ALTER TABLE pgokf_private.access_log ADD CONSTRAINT access_log_op_check
    CHECK (op IN ('export_parquet', 'export_sources', 'get_concept_source',
                  'get_skill', 'get_script', 'get_reference'));
COMMENT ON COLUMN pgokf_private.access_log.op IS
    'The exfiltration operation: export_parquet / export_sources / get_concept_source / get_skill / get_script / get_reference.';

-- ===========================================================================
-- 5. Link comments: edges may now be resolved by target path (a package
--    resource keeps its extension in its id), and package-membership edges
--    use a new link_kind and two new relations.
COMMENT ON COLUMN pgokf.links.target_id IS
    'Concept ID of an internal destination: target_path without .md, or, when a concept exists at exactly target_path under another id (a skill package resource keeps its extension), that concept''s id; NULL for external destinations.';
COMMENT ON COLUMN pgokf.links.link_kind IS
    'Markdown construct that produced the link: inline, reference, autolink, email, or image; or package for a skill package''s membership edge to a resource it owns (no Markdown construct, the relation is in link_relation).';
COMMENT ON COLUMN pgokf.links.link_relation IS
    'Semantic relation the edge represents, distinct from the Markdown construct in link_kind. ''reference'' (the default) for every ordinary Markdown link; for an Attested Computation concept''s type-specific reference fields, ''attestation:computation'', ''attestation:executor'', or ''attestation:attester''; for a skill package, USES (skill -> script) and REFERENCES (skill -> reference or asset), on both the membership edges and the manifest''s own Markdown links to those resources, so a reader can SELECT the typed edges while concept_neighbors traverses them like any resolved internal edge.';

-- Last, so the three new relations are registered for pg_dump (the rule for
-- every upgrade script since 0.1.14).
SELECT pgokf_private.register_dump_relations();
