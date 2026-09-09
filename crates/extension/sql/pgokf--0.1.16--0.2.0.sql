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

-- ===========================================================================
-- The web UI's identity state (the web_identity_tables block of
-- src/catalog/web_identity.rs, verbatim). The people a local sign-in knows
-- and the sessions the UI has issued live in the catalog, not in files beside
-- it: a change is transactional, every UI instance sees the same state, and
-- pg_dump carries them. Neither is catalog content, so neither is
-- tenant-scoped. pgokf_writer (and so pgokf_admin) reads and writes both;
-- pgokf_reader sees neither, since it must not learn password hashes or
-- session identifiers. The extension itself never reads them.
CREATE SCHEMA pgokf_web;
REVOKE ALL ON SCHEMA pgokf_web FROM PUBLIC;
GRANT USAGE ON SCHEMA pgokf_web TO pgokf_writer;
COMMENT ON SCHEMA pgokf_web IS
    'The web UI''s identity state: the people a local sign-in knows, the sessions the UI has issued, the bearer tokens the MCP server accepts over HTTP, and the identity providers an admin set up. Owned by the extension so it is transactional, shared by every UI instance, and dumped with the catalog; read and written by pgokf_writer only, and never by the extension itself.';

CREATE TABLE pgokf_web.users (
    name          text        NOT NULL,
    role          text        NOT NULL
        CONSTRAINT users_role_check
        CHECK (role IN ('viewer', 'uploader', 'editor', 'approver', 'admin')),
    password_hash text,
    display_name  text
        CONSTRAINT users_display_name_check
        CHECK (display_name ~ '^[^[:cntrl:]]+$' AND char_length(display_name) <= 256),
    provider      text
        CONSTRAINT users_provider_check CHECK (provider ~ '^[a-z0-9][a-z0-9-]{0,31}$'),
    created_at    timestamptz NOT NULL DEFAULT now(),
    updated_at    timestamptz NOT NULL DEFAULT now(),
    CONSTRAINT users_pkey PRIMARY KEY (name),
    CONSTRAINT users_name_check CHECK (name ~ '^[A-Za-z0-9._@+-]{1,128}$'),
    CONSTRAINT users_sign_in_check CHECK ((password_hash IS NULL) <> (provider IS NULL))
);

CREATE TABLE pgokf_web.sessions (
    nonce      text        NOT NULL,
    subject    text        NOT NULL,
    mode       text        NOT NULL
        CONSTRAINT sessions_mode_check CHECK (mode IN ('users', 'oidc')),
    provider   text
        CONSTRAINT sessions_provider_check CHECK (provider ~ '^[a-z0-9][a-z0-9-]{0,31}$'),
    expires_at timestamptz NOT NULL,
    created_at timestamptz NOT NULL DEFAULT now(),
    CONSTRAINT sessions_pkey PRIMARY KEY (nonce),
    CONSTRAINT sessions_provider_mode_check CHECK (provider IS NULL OR mode = 'oidc')
);
CREATE INDEX sessions_subject_idx ON pgokf_web.sessions (subject);
CREATE INDEX sessions_expires_at_idx ON pgokf_web.sessions (expires_at);

REVOKE ALL ON TABLE pgokf_web.users, pgokf_web.sessions FROM PUBLIC;
GRANT SELECT, INSERT, UPDATE, DELETE ON TABLE pgokf_web.users, pgokf_web.sessions TO pgokf_writer;

COMMENT ON TABLE pgokf_web.users IS
    'People the web UI''s users identity mode signs in: one row per person with their role on the viewer < uploader < editor < approver < admin ladder and an Argon2id hash of their password - or, for a person an identity provider signed in, no password and the provider that brought them, their row appearing at their first sign-in. A name belongs to exactly one way in. Managed by pgokf-web (its user add / set-password commands and the Admin page); pgokf_writer only, so a reader never sees a hash.';
COMMENT ON COLUMN pgokf_web.users.name IS
    'The sign-in name: one plain token of letters, digits, and . _ @ + - (at most 128), which is also the person''s OKF actor (human:<name>). For a person an identity provider signed in, the identity claim the provider was set up with (sub, login, email...).';
COMMENT ON COLUMN pgokf_web.users.role IS
    'The role the UI grants on every request: viewer, uploader, editor, approver, or admin (each holds everything below it). A session cookie carries no role, so a change here takes effect at once. A person an identity provider signed in holds the higher of this role and the one their groups map to.';
COMMENT ON COLUMN pgokf_web.users.password_hash IS
    'An Argon2id PHC string of the password, or NULL for a person an identity provider signed in: they have no password here, a password sign-in under their name is refused, and their row exists so an admin can see them and set their role. A fingerprint of the hash is bound into every session a password opens, so a changed password ends the sessions opened before it.';
COMMENT ON COLUMN pgokf_web.users.display_name IS
    'What the person is called wherever the UI shows them: for a person an identity provider signed in, the name the provider reports, refreshed at every sign-in; for a password person, what an admin entered, or NULL to show the sign-in name. Their OKF actor stays human:<name>.';
COMMENT ON COLUMN pgokf_web.users.provider IS
    'For a person without a password, the identity provider that signed them in (identity_providers.id); NULL for a password person. With users_sign_in_check this makes a name belong to exactly one way in: a provider never signs in a password person''s name, nor a name another provider brought.';
COMMENT ON COLUMN pgokf_web.users.created_at IS 'When the person was added, or first signed in.';
COMMENT ON COLUMN pgokf_web.users.updated_at IS 'When the role, password, or name last changed.';

COMMENT ON TABLE pgokf_web.sessions IS
    'The sessions the web UI has issued and not yet ended, in either local identity mode (users or oidc). A session cookie is signed, so the UI could always verify one but never forget one; this table is its memory: a cookie whose nonce is not here is refused, so signing out, sign out everywhere, or an admin ending someone''s sessions takes effect on every device at once. pgokf_writer only: a session identifier is not for readers.';
COMMENT ON COLUMN pgokf_web.sessions.nonce IS 'The random session identifier the signed cookie carries.';
COMMENT ON COLUMN pgokf_web.sessions.subject IS 'Whose session it is: the users-mode name or the provider''s subject claim.';
COMMENT ON COLUMN pgokf_web.sessions.mode IS 'The identity mode that opened it (users or oidc); a mode never honours the other''s sessions.';
COMMENT ON COLUMN pgokf_web.sessions.provider IS 'For a session opened by an identity provider set up on the Admin page: that provider (identity_providers.id), so switching it off or removing it ends its sessions alone; NULL for a password session, or one the oidc mode''s own provider opened.';
COMMENT ON COLUMN pgokf_web.sessions.expires_at IS 'When the session ends by itself; expired rows are pruned as new sessions are opened.';
COMMENT ON COLUMN pgokf_web.sessions.created_at IS 'When the person signed in.';

CREATE TABLE pgokf_web.mcp_tokens (
    name       text        NOT NULL,
    role       text        NOT NULL
        CONSTRAINT mcp_tokens_role_check
        CHECK (role IN ('reader', 'builder', 'writer', 'admin')),
    tenant     text
        CONSTRAINT mcp_tokens_tenant_check CHECK (tenant ~ '^[^[:cntrl:]]{1,128}$'),
    digest     text        NOT NULL
        CONSTRAINT mcp_tokens_digest_check CHECK (digest ~ '^[0-9a-f]{64}$'),
    created_by text        NOT NULL,
    created_at timestamptz NOT NULL DEFAULT now(),
    CONSTRAINT mcp_tokens_pkey PRIMARY KEY (digest),
    CONSTRAINT mcp_tokens_name_key UNIQUE NULLS NOT DISTINCT (tenant, name),
    CONSTRAINT mcp_tokens_name_check CHECK (name ~ '^[A-Za-z0-9._@+-]{1,128}$')
);
REVOKE ALL ON TABLE pgokf_web.mcp_tokens FROM PUBLIC;
GRANT SELECT, INSERT, DELETE ON TABLE pgokf_web.mcp_tokens TO pgokf_writer;

COMMENT ON TABLE pgokf_web.mcp_tokens IS
    'The bearer tokens that may call pgokf-mcp over HTTP: one row per token with what to call it in the log, its role (reader searches and reads; builder may also build workspace plugins), the tenant it was minted for, and the SHA-256 digest of the token - never the token, which is shown once when it is minted. The digest is the token''s identity; a name is unique within its tenant. Minted and revoked by pgokf-web (the Admin page, or its mcp-token command), each UI seeing its own tenant''s tokens; pgokf_writer only. A reader learns the bearer of one digest through pgokf.mcp_token_bearer(), and nothing else.';
COMMENT ON COLUMN pgokf_web.mcp_tokens.name IS
    'What the token is called in the log beside every call it makes: one plain token of letters, digits, and . _ @ + - (at most 128), unique within its tenant.';
COMMENT ON COLUMN pgokf_web.mcp_tokens.role IS
    'What the token may do, as a ladder where each role holds everything below it: reader (search and read the catalog), builder (also build workspace plugins), writer (also write documents into a content bundle - what an agent writes arrives unverified whatever it claims, so a person still reviews it), admin (also register, refresh, enable, disable, retire, and unregister bundles). The MCP server decides per tool from this, and the two writing roles need it to hold a writer connection of its own.';
COMMENT ON COLUMN pgokf_web.mcp_tokens.tenant IS
    'The tenant the token was minted for - the pgokf.tenant scope of the UI or command that minted it - or NULL for a catalog served without one; one to 128 printable characters. An MCP endpoint accepts only tokens minted for its own tenant, so one process serves one tenant with tokens of its own; this is a label the server checks, not a policy the database enforces.';
COMMENT ON COLUMN pgokf_web.mcp_tokens.digest IS
    'The SHA-256 of the token, as 64 lower-case hex characters, and the row''s identity. A token is 256 random bits, so a fast hash is the right way to store it; the token itself is never kept.';
COMMENT ON COLUMN pgokf_web.mcp_tokens.created_by IS 'Who minted it: the admin''s sign-in name or subject, or cli.';
COMMENT ON COLUMN pgokf_web.mcp_tokens.created_at IS 'When it was minted.';

CREATE TABLE pgokf_web.identity_providers (
    id             text        NOT NULL
        CONSTRAINT identity_providers_id_check CHECK (id ~ '^[a-z0-9][a-z0-9-]{0,31}$'),
    enabled        boolean     NOT NULL DEFAULT true,
    kind           text        NOT NULL DEFAULT 'oidc'
        CONSTRAINT identity_providers_kind_check CHECK (kind IN ('oidc', 'github')),
    issuer         text        NOT NULL
        CONSTRAINT identity_providers_issuer_check
        CHECK (issuer ~ '^https?://[^[:space:][:cntrl:]]+$' AND char_length(issuer) <= 2048),
    client_id      text        NOT NULL
        CONSTRAINT identity_providers_client_id_check
        CHECK (client_id ~ '^[^[:cntrl:]]+$' AND char_length(client_id) <= 512),
    client_secret  text
        CONSTRAINT identity_providers_client_secret_check CHECK (client_secret ~ '^v1:[A-Za-z0-9_-]{16}:[A-Za-z0-9_-]{22,}$'),
    redirect_url   text        NOT NULL
        CONSTRAINT identity_providers_redirect_url_check
        CHECK (redirect_url ~ '^https?://[^[:space:][:cntrl:]]+$' AND char_length(redirect_url) <= 2048),
    scopes         text        NOT NULL DEFAULT 'openid profile email'
        CONSTRAINT identity_providers_scopes_check CHECK (scopes ~ '^[^[:cntrl:]]*$' AND char_length(scopes) <= 512),
    subject_claims text        NOT NULL DEFAULT 'sub'
        CONSTRAINT identity_providers_subject_claims_check
        CHECK (subject_claims ~ '^[^[:cntrl:]]+$' AND char_length(subject_claims) <= 512),
    groups_claim   text        NOT NULL DEFAULT 'groups'
        CONSTRAINT identity_providers_groups_claim_check CHECK (groups_claim ~ '^[^[:space:][:cntrl:]]{1,128}$'),
    provider_name  text        NOT NULL
        CONSTRAINT identity_providers_provider_name_check CHECK (provider_name ~ '^[^[:cntrl:]]{1,64}$'),
    role_map       text        NOT NULL DEFAULT ''
        CONSTRAINT identity_providers_role_map_check CHECK (role_map ~ '^[^[:cntrl:]]*$' AND char_length(role_map) <= 4096),
    default_role   text        NOT NULL DEFAULT 'viewer'
        CONSTRAINT identity_providers_default_role_check
        CHECK (default_role IN ('viewer', 'uploader', 'editor', 'approver', 'admin')),
    created_at     timestamptz NOT NULL DEFAULT now(),
    updated_at     timestamptz NOT NULL DEFAULT now(),
    updated_by     text        NOT NULL,
    CONSTRAINT identity_providers_pkey PRIMARY KEY (id)
);
CREATE UNIQUE INDEX identity_providers_provider_name_idx
    ON pgokf_web.identity_providers (lower(provider_name));
REVOKE ALL ON TABLE pgokf_web.identity_providers FROM PUBLIC;
GRANT SELECT, INSERT, UPDATE, DELETE ON TABLE pgokf_web.identity_providers TO pgokf_writer;

COMMENT ON TABLE pgokf_web.identity_providers IS
    'The identity providers the web UI signs people in against, as an admin set them up on the Admin page (the users mode''s own sign-in stays beside them): any number, each an OpenID Connect provider or GitHub - github.com or a GitHub Enterprise Server - by its OAuth web flow, each with its own button on the sign-in page. The client secret is stored sealed by pgokf-web under a key derived from its session secret, so the catalog - and any writer credential - holds ciphertext, never the secret. pgokf_writer only.';
COMMENT ON COLUMN pgokf_web.identity_providers.id IS 'A short slug made from the provider''s name when it was added: its handle in the sign-in URL, in the sessions it opens, and on the people it signed in. It never changes, even when the name does.';
COMMENT ON COLUMN pgokf_web.identity_providers.enabled IS 'Whether the provider is offered on the sign-in page. Off keeps the settings for later.';
COMMENT ON COLUMN pgokf_web.identity_providers.kind IS 'What the provider speaks: oidc (OpenID Connect discovery and an ID token verified against the provider''s published keys) or github (GitHub''s OAuth web flow: the person from /user, their verified email from /user/emails, their groups from the organizations and org/team slugs they belong to).';
COMMENT ON COLUMN pgokf_web.identity_providers.issuer IS 'The issuer URL, exactly as the provider declares it in its discovery document - or, for GitHub, the GitHub host: https://github.com, or a GitHub Enterprise Server.';
COMMENT ON COLUMN pgokf_web.identity_providers.client_id IS 'The client id this site is registered with at the provider.';
COMMENT ON COLUMN pgokf_web.identity_providers.client_secret IS 'The client secret for a confidential client, sealed (v1:<nonce>:<ciphertext>, AES-256-GCM under a key derived from the UI''s session secret); NULL for a public client, which PKCE alone protects. The constraint refuses anything that is not the sealed form, so a plaintext secret can never be stored.';
COMMENT ON COLUMN pgokf_web.identity_providers.redirect_url IS 'This site''s callback URL (<site>/auth/callback, shared by every provider), as registered with the provider.';
COMMENT ON COLUMN pgokf_web.identity_providers.scopes IS 'The scopes asked for, space-separated; openid is always included for an OpenID Connect provider, and GitHub takes its own (read:user user:email read:org).';
COMMENT ON COLUMN pgokf_web.identity_providers.subject_claims IS 'The claims tried in order for the person''s identity, comma-separated (sub is always stable; email only when the provider says it is verified). For GitHub the claims are sub (the numeric account id), login, name, and email.';
COMMENT ON COLUMN pgokf_web.identity_providers.groups_claim IS 'The claim carrying the person''s groups, which the role map turns into a role; for GitHub, the organizations and org/team slugs the person belongs to, under this name.';
COMMENT ON COLUMN pgokf_web.identity_providers.provider_name IS 'What the sign-in button calls the provider; unique among the providers, case aside.';
COMMENT ON COLUMN pgokf_web.identity_providers.role_map IS 'group=role entries, comma-separated; the highest matching role wins.';
COMMENT ON COLUMN pgokf_web.identity_providers.default_role IS 'The role of a person in no mapped group.';
COMMENT ON COLUMN pgokf_web.identity_providers.created_at IS 'When the provider was added.';
COMMENT ON COLUMN pgokf_web.identity_providers.updated_at IS 'When the settings last changed; every UI instance notices a change through it.';
COMMENT ON COLUMN pgokf_web.identity_providers.updated_by IS 'The admin who last changed them.';

CREATE FUNCTION pgokf.mcp_token_bearer(digest text)
RETURNS TABLE (name text, role text, tenant text)
LANGUAGE sql STABLE STRICT
SECURITY DEFINER SET search_path = pg_catalog, pg_temp
AS $mcp_token_bearer$
    SELECT t.name, t.role, t.tenant
    FROM pgokf_web.mcp_tokens AS t
    WHERE t.digest = mcp_token_bearer.digest
$mcp_token_bearer$;
REVOKE ALL ON FUNCTION pgokf.mcp_token_bearer(text) FROM PUBLIC;
GRANT EXECUTE ON FUNCTION pgokf.mcp_token_bearer(text) TO pgokf_reader;
COMMENT ON FUNCTION pgokf.mcp_token_bearer(text) IS
    'The name, role, and tenant of the MCP token whose SHA-256 digest this is, or no row; the server accepts only a token minted for its own tenant. How pgokf-mcp, which connects as a reader, authenticates a request over HTTP: it hashes the presented token itself and asks for that digest, so the token never travels to the database and a reader learns the bearer of a digest it holds and nothing about any other. SECURITY DEFINER over pgokf_web.mcp_tokens, which no reader may see; STABLE, STRICT, executable by pgokf_reader. A revoked token is refused with the very next request: nothing is cached.';

-- Last, so the eight new relations are registered for pg_dump (the rule for
-- every upgrade script since 0.1.14).
SELECT pgokf_private.register_dump_relations();
