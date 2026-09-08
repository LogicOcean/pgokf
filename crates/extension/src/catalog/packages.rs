// SPDX-License-Identifier: AGPL-3.0-only
//! Skill packages: staging, exact-payload projections, and retrieval.
//!
//! An Agent Skills package (a directory with a `SKILL.md` and optional
//! `scripts/`, `references/`, and `assets/`) is catalog content that an agent
//! plugin must be able to reproduce byte for byte. The generic
//! `pgokf.concepts` row indexes a package member for search and the graph;
//! this module owns the *exact* side (specification §5.3, §15-§18):
//!
//! - `pgokf.skills` keeps the manifest's original bytes, its complete
//!   frontmatter, and the §9 package hash, whether or not `store_source` is on;
//! - `pgokf.scripts` keeps a script's exact UTF-8 bytes, language, and SHA-256;
//! - `pgokf.reference_documents` keeps a reference or asset's exact bytes,
//!   media type, and SHA-256, plus its text when it is textual.
//!
//! # Staging
//!
//! The sync engine classifies every snapshot entry ([`okf_sync::FileClass`])
//! and hands this module the ones that belong to packages. A manifest is
//! parsed by [`okf_parser::parse_skill_manifest`] into a virtual
//! `type: Skill` concept; a script or reference becomes a virtual `Script` /
//! `Reference` concept built directly from its bytes ([`stage_script`],
//! [`stage_reference`]). Each carries a [`TypedPayload`] for the projection.
//! A package is invalidated as a whole when any member changes
//! ([`invalidate_packages`]), so the skill's package hash and membership
//! edges are always recomputed from the current snapshot.
//!
//! # Projection and graph
//!
//! [`project`] runs inside the sync transaction after the generic rows and the
//! body links are written: it upserts the typed rows, adds one membership edge
//! per owned resource (`USES` for scripts, `REFERENCES` for references and
//! assets), and re-labels the manifest's own Markdown links to those resources
//! with the same relations. Removals need no seam: every typed row cascades
//! from `pgokf.concepts`.
//!
//! # Retrieval
//!
//! `get_skill`, `get_script`, and `get_reference` are reader-level,
//! `SECURITY DEFINER`, tenant-scoped, and audited in
//! `pgokf_private.access_log` like `get_concept_source`: they return the exact
//! stored bytes, never `body_text`.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use okf_parser::{Map, ParsedConcept, ParserLimits, Value, parse_skill_manifest, validate_skill};
use okf_sync::{FileClass, FileMetadata, PackageIndex, hash_bytes};
use pgrx::heap_tuple::PgHeapTuple;
use pgrx::{AllocatedByRust, Spi, extension_sql};

use crate::catalog::batch::BATCH_SIZE;
use crate::catalog::spi_read::RowReader;
use crate::catalog::sync::BODY_TSV_CHAR_LIMIT;
use crate::catalog::types::{
    PackageMember, ReferencePayload, ScriptPayload, SkillPayload, StagedConcept, TypedPayload,
};
use crate::errors::CatalogError;
use crate::security;

/// The concept type of a package script.
pub const SCRIPT_TYPE: &str = "Script";
/// The concept type of a package reference or asset.
pub const REFERENCE_TYPE: &str = "Reference";
/// The link relation from a skill to a script it owns.
pub const USES_RELATION: &str = "USES";
/// The link relation from a skill to a reference or asset it owns.
pub const REFERENCES_RELATION: &str = "REFERENCES";
/// The `link_kind` of a package-membership edge (not a Markdown construct).
pub const MEMBERSHIP_LINK_KIND: &str = "package";
/// The default visibility of a package and its members.
pub const DEFAULT_VISIBILITY: &str = "internal";
/// Version of the classification rules folded into every package hash.
const CLASSIFIER_VERSION: &str = "1";
/// Version of the manifest/resource validation rules folded into every
/// package hash (specification §9), so a rule change re-projects packages.
const VALIDATOR_VERSION: &str = "1";
/// Domain separator of the package hash.
const PACKAGE_HASH_DOMAIN: &str = "pgokf.package:v1";
/// The three visibility values the projections accept.
const VISIBILITIES: [&str; 3] = ["public", "internal", "private"];

const SKILL_RESULT_TYPE: &str = "pgokf.skill_result";
const SCRIPT_RESULT_TYPE: &str = "pgokf.script_result";
const REFERENCE_RESULT_TYPE: &str = "pgokf.reference_result";

extension_sql!(
    r"
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
",
    name = "package_tables",
    requires = ["catalog_tables"]
);

fn spi_error(context: &str, error: &pgrx::spi::Error) -> CatalogError {
    CatalogError::internal(format!("{context}: {error}"), Path::new(""))
}

fn composite_error(type_name: &str, error: impl std::fmt::Display) -> CatalogError {
    CatalogError::internal(
        format!("failed to build {type_name} composite: {error}"),
        Path::new(""),
    )
}

// ---------------------------------------------------------------------------
// Staging: pure transforms from snapshot entries and bytes to staged concepts.
// ---------------------------------------------------------------------------

/// Pull every skill package's manifest into `to_parse` when its membership
/// changed this sync, so the package hash and membership edges are
/// recomputed from the current snapshot (§9).
///
/// A package is touched when a resource it owns now (by the current
/// snapshot) or owned before (by `stored_owners`, `path -> owning skill id`
/// from the typed projections) is added, changed, or removed, and when a
/// manifest is added or removed inside it: a nested `SKILL.md` appearing or
/// disappearing moves every resource below it between the two packages,
/// so the enclosing package is touched too. A manifest already scheduled is
/// left alone; a touched manifest that is unchanged as a file is appended
/// (not counted as `updated` in the file report, but listed in the
/// per-concept change manifest as a dependency invalidation).
pub fn invalidate_packages(
    to_parse: &mut Vec<FileMetadata>,
    removed_paths: &[String],
    current: &[FileMetadata],
    stored_owners: &BTreeMap<String, String>,
) {
    let index = PackageIndex::from_paths(
        current
            .iter()
            .filter(|entry| entry.class == FileClass::SkillManifest)
            .map(|entry| entry.path.to_str().unwrap_or_default()),
    );
    let scheduled: BTreeSet<String> = to_parse
        .iter()
        .map(|entry| entry.path.to_string_lossy().into_owned())
        .collect();
    let mut touched_roots: BTreeSet<String> = BTreeSet::new();
    for path in scheduled
        .iter()
        .map(String::as_str)
        .chain(removed_paths.iter().map(String::as_str))
    {
        if let Some(previous_owner) = stored_owners.get(path) {
            touched_roots.insert(package_root_of(previous_owner).to_owned());
        }
        if let Some(root) = index.owner_of(path)
            && index.classify(path).is_some_and(FileClass::is_resource)
        {
            touched_roots.insert(root.to_owned());
        }
        if let Some(directory) = PackageIndex::manifest_directory(path) {
            // A manifest came or went at `directory`: the package that
            // encloses it gains or loses everything below it.
            if let Some(enclosing) = index.owner_of(directory) {
                touched_roots.insert(enclosing.to_owned());
            }
        }
    }
    for entry in current {
        if entry.class != FileClass::SkillManifest {
            continue;
        }
        let path = entry.path.to_string_lossy();
        let Some(root) = PackageIndex::manifest_directory(&path) else {
            continue;
        };
        if touched_roots.contains(root) && !scheduled.contains(path.as_ref()) {
            to_parse.push(entry.clone());
        }
    }
}

/// The package root a skill concept id names (`skills/deploy/SKILL` ->
/// `skills/deploy`; `SKILL` -> `""`).
#[must_use]
pub fn package_root_of(skill_concept_id: &str) -> &str {
    skill_concept_id
        .rsplit_once('/')
        .map_or("", |(root, _)| root)
}

/// The skill concept id of the package rooted at `root`.
#[must_use]
pub fn skill_concept_id_of(root: &str) -> String {
    let stem = okf_sync::SKILL_MANIFEST
        .strip_suffix(".md")
        .unwrap_or(okf_sync::SKILL_MANIFEST);
    if root.is_empty() {
        stem.to_owned()
    } else {
        format!("{root}/{stem}")
    }
}

/// The members of the package rooted at `root`, from the current snapshot,
/// in path order.
#[must_use]
pub fn package_members(
    root: &str,
    index: &PackageIndex,
    current: &[FileMetadata],
) -> Vec<PackageMember> {
    let mut members: Vec<PackageMember> = current
        .iter()
        .filter(|entry| entry.class.is_resource())
        .filter_map(|entry| {
            let path = entry.path.to_str()?;
            if index.owner_of(path) != Some(root) {
                return None;
            }
            let package_path = index.package_relative(path)?;
            Some(PackageMember {
                class: entry.class,
                path: path.to_owned(),
                package_path: package_path.to_owned(),
                concept_id: entry.class.concept_id(path),
                file_hash: entry.hash.clone(),
            })
        })
        .collect();
    members.sort_by(|a, b| a.path.cmp(&b.path));
    members
}

/// The §9 package hash: BLAKE3 over a domain-separated encoding of the
/// classifier and validator versions, the manifest's file hash, and each
/// member as `class NUL package-relative-path NUL byte-hash LF`, sorted by
/// path.
#[must_use]
pub fn package_hash(skill_md_hash: &str, members: &[PackageMember]) -> String {
    let mut buffer = String::new();
    buffer.push_str(PACKAGE_HASH_DOMAIN);
    buffer.push('\n');
    buffer.push_str(CLASSIFIER_VERSION);
    buffer.push('\n');
    buffer.push_str(VALIDATOR_VERSION);
    buffer.push('\n');
    buffer.push_str(skill_md_hash);
    buffer.push('\n');
    let mut sorted: Vec<&PackageMember> = members.iter().collect();
    sorted.sort_by(|a, b| a.package_path.cmp(&b.package_path));
    for member in sorted {
        buffer.push_str(member.class.label());
        buffer.push('\0');
        buffer.push_str(&member.package_path);
        buffer.push('\0');
        buffer.push_str(&member.file_hash);
        buffer.push('\n');
    }
    hash_bytes(buffer.as_bytes())
}

/// The visibility a manifest declares, when it is one of the three values;
/// `internal` otherwise.
#[must_use]
pub fn skill_visibility(agent_skill: &Value) -> String {
    agent_skill
        .get("visibility")
        .and_then(Value::as_str)
        .filter(|v| VISIBILITIES.contains(v))
        .unwrap_or(DEFAULT_VISIBILITY)
        .to_owned()
}

/// Parse a `SKILL.md` and stage it with its package identity.
///
/// `members` are the package's current resources (see
/// [`package_members`]); `file_hash` is the manifest's snapshot hash.
/// Structural findings against the Agent Skills standard are returned
/// alongside so the caller can log them; they never fail the parse.
///
/// # Errors
///
/// The manifest cannot be parsed (see [`parse_skill_manifest`]).
pub fn stage_manifest(
    bytes: Vec<u8>,
    path: &Path,
    file_hash: &str,
    modified_at_epoch: Option<f64>,
    members: Vec<PackageMember>,
    limits: ParserLimits,
    store_source: bool,
) -> Result<(StagedConcept, Vec<String>), okf_parser::Error> {
    let concept = parse_skill_manifest(&bytes, path, limits)?;
    let package_root = okf_parser::parent_directory(&concept.path).to_owned();
    let findings: Vec<String> = validate_skill(&concept, &package_root)
        .iter()
        .map(|d| format!("{} ({})", d, d.code()))
        .collect();
    let agent_skill = concept
        .metadata
        .get(okf_parser::skill::AGENT_SKILL_KEY)
        .cloned()
        .unwrap_or(Value::Null);
    let payload = SkillPayload {
        package_hash: package_hash(file_hash, &members),
        package_root,
        skill_md: bytes.clone(),
        visibility: skill_visibility(&agent_skill),
        agent_skill,
        members,
    };
    Ok((
        StagedConcept {
            concept,
            file_hash: file_hash.to_owned(),
            modified_at_epoch,
            raw_content: store_source.then_some(bytes),
            typed: Some(TypedPayload::Skill(payload)),
        },
        findings,
    ))
}

/// Everything a virtual resource concept is built from besides its bytes.
#[derive(Debug, Clone)]
pub struct ResourceContext<'a> {
    /// Normalized bundle-relative path.
    pub path: &'a str,
    /// Path relative to the package root.
    pub package_path: &'a str,
    /// The owning skill's concept ID.
    pub package_concept_id: &'a str,
    /// Visibility inherited from the owning skill.
    pub visibility: &'a str,
    /// Snapshot BLAKE3 hash of the bytes.
    pub file_hash: &'a str,
    /// Filesystem modification time as epoch seconds, when known.
    pub modified_at_epoch: Option<f64>,
}

/// Stage a file below `scripts/` as a virtual `type: Script` concept.
///
/// # Errors
///
/// The bytes are not valid UTF-8: a binary under `scripts/` is never a
/// script (move it to `assets/`).
pub fn stage_script(
    bytes: Vec<u8>,
    context: &ResourceContext<'_>,
    store_source: bool,
) -> Result<StagedConcept, CatalogError> {
    let text = std::str::from_utf8(&bytes)
        .ok()
        // PostgreSQL's `text` cannot hold U+0000, and a script is stored as
        // text as well as bytes, so one would either abort the sync or
        // leave the two disagreeing. It is not a script; it is a binary.
        .filter(|text| !text.contains('\0'))
        .ok_or_else(|| {
            CatalogError::invalid_parameter(
                format!(
                    "{} is not text a script can be stored as; a binary file below scripts/ is \
                     not a Script (place it under assets/ instead)",
                    context.path
                ),
                Path::new(context.path),
            )
        })?;
    let (language, runtime) = infer_language(context.package_path, text);
    let mut metadata = resource_metadata(context, "script");
    metadata.insert("language".to_owned(), Value::String(language.clone()));
    if let Some(runtime) = &runtime {
        metadata.insert("runtime".to_owned(), runtime.clone());
    }
    let concept = ParsedConcept {
        id: context.path.to_owned(),
        declared_id: None,
        path: context.path.to_owned(),
        r#type: SCRIPT_TYPE.to_owned(),
        title: file_name(context.package_path).to_owned(),
        description: script_description(text),
        tags: Vec::new(),
        resource: None,
        body_text: text.to_owned(),
        links: Vec::new(),
        metadata,
    };
    let payload = ScriptPayload {
        language,
        runtime,
        source_path: context.package_path.to_owned(),
        package_concept_id: context.package_concept_id.to_owned(),
        visibility: context.visibility.to_owned(),
        bytes: bytes.clone(),
    };
    Ok(StagedConcept {
        concept,
        file_hash: context.file_hash.to_owned(),
        modified_at_epoch: context.modified_at_epoch,
        raw_content: store_source.then_some(bytes),
        typed: Some(TypedPayload::Script(payload)),
    })
}

/// Stage a file below `references/` or `assets/` as a virtual
/// `type: Reference` concept, textual or binary.
#[must_use]
pub fn stage_reference(
    bytes: Vec<u8>,
    context: &ResourceContext<'_>,
    store_source: bool,
) -> StagedConcept {
    let (media_type, format) = media_type_for(context.package_path, &bytes);
    let text_body = if format == "binary" || format == "image" {
        None
    } else {
        std::str::from_utf8(&bytes)
            .ok()
            // PostgreSQL's `text` cannot hold U+0000. The exact bytes are
            // kept either way; only the searchable text is dropped, so the
            // two never disagree about what the file contains.
            .filter(|text| !text.contains('\0'))
            .map(str::to_owned)
    };
    let (title, body_text) = match text_body.as_deref() {
        Some(text) if format == "markdown" => {
            // A Markdown reference may open with a frontmatter block; it is
            // metadata, not prose, so neither the title nor the search text
            // comes from it.
            let body = okf_parser::frontmatter::split(text, context.path, text.len())
                .map_or(text, |(_, body)| body);
            (
                first_heading(body).unwrap_or_else(|| file_name(context.package_path).to_owned()),
                okf_parser::markdown::plain_text(body),
            )
        }
        Some(text) => (file_name(context.package_path).to_owned(), text.to_owned()),
        None => (file_name(context.package_path).to_owned(), String::new()),
    };
    let origin = if context.package_path.starts_with("assets/") {
        "asset"
    } else {
        "reference"
    };
    let mut metadata = resource_metadata(context, origin);
    metadata.insert("format".to_owned(), Value::String(format.clone()));
    metadata.insert("media_type".to_owned(), Value::String(media_type.clone()));
    let concept = ParsedConcept {
        id: context.path.to_owned(),
        declared_id: None,
        path: context.path.to_owned(),
        r#type: REFERENCE_TYPE.to_owned(),
        title,
        description: None,
        tags: Vec::new(),
        resource: None,
        body_text,
        links: Vec::new(),
        metadata,
    };
    let payload = ReferencePayload {
        format,
        media_type,
        text_body,
        source_path: context.package_path.to_owned(),
        package_concept_id: context.package_concept_id.to_owned(),
        visibility: context.visibility.to_owned(),
        bytes: bytes.clone(),
    };
    StagedConcept {
        concept,
        file_hash: context.file_hash.to_owned(),
        modified_at_epoch: context.modified_at_epoch,
        raw_content: store_source.then_some(bytes),
        typed: Some(TypedPayload::Reference(payload)),
    }
}

/// The producer-style metadata every virtual resource carries: where it came
/// from inside its package and which skill owns it.
fn resource_metadata(context: &ResourceContext<'_>, class: &str) -> Map<String, Value> {
    let mut metadata = Map::new();
    metadata.insert(
        "package".to_owned(),
        Value::String(context.package_concept_id.to_owned()),
    );
    metadata.insert(
        "source_path".to_owned(),
        Value::String(context.package_path.to_owned()),
    );
    metadata.insert("resource_class".to_owned(), Value::String(class.to_owned()));
    metadata.insert(
        "visibility".to_owned(),
        Value::String(context.visibility.to_owned()),
    );
    metadata
}

/// The basename of a package-relative path.
fn file_name(package_path: &str) -> &str {
    package_path
        .rsplit_once('/')
        .map_or(package_path, |(_, name)| name)
}

/// A script's description: its first comment line after the shebang, when
/// the file opens with a `#` or `//` comment.
fn script_description(text: &str) -> Option<String> {
    text.lines()
        .filter(|line| !line.starts_with("#!"))
        .map(str::trim)
        .take_while(|line| line.starts_with('#') || line.starts_with("//"))
        .map(|line| line.trim_start_matches(['#', '/']).trim())
        .find(|line| !line.is_empty())
        .map(str::to_owned)
}

/// The first Markdown ATX heading of a text, without its marker, skipping
/// fenced code blocks (a `# comment` inside a fence is not a heading).
fn first_heading(text: &str) -> Option<String> {
    let mut fence: Option<usize> = None;
    for line in text.lines().map(str::trim) {
        let ticks = line.len() - line.trim_start_matches(['`', '~']).len();
        if ticks >= 3 && (line.starts_with("```") || line.starts_with("~~~")) {
            fence = match fence {
                Some(open) if ticks >= open => None,
                Some(open) => Some(open),
                None => Some(ticks),
            };
            continue;
        }
        if fence.is_some() {
            continue;
        }
        if let Some(rest) = line.strip_prefix('#') {
            let title = rest.trim_start_matches('#').trim();
            if !title.is_empty() {
                return Some(title.to_owned());
            }
        }
    }
    None
}

/// Infer a script's language and interpreter: the shebang decides when there
/// is one, the extension otherwise; `unknown` when neither is recognized.
#[must_use]
pub fn infer_language(package_path: &str, text: &str) -> (String, Option<Value>) {
    let shebang = text
        .lines()
        .next()
        .and_then(|line| line.strip_prefix("#!"))
        .map(str::trim)
        .filter(|line| !line.is_empty());
    let runtime = shebang.map(|executable| {
        let mut map = Map::new();
        map.insert(
            "executable".to_owned(),
            Value::String(executable.to_owned()),
        );
        Value::Object(map)
    });
    let from_shebang = shebang.and_then(language_from_shebang);
    let from_extension = Path::new(package_path)
        .extension()
        .and_then(|e| e.to_str())
        .and_then(language_from_extension);
    let language = from_shebang
        .or(from_extension)
        .unwrap_or("unknown")
        .to_owned();
    (language, runtime)
}

/// The language a shebang interpreter names (`/usr/bin/env bash` → bash).
fn language_from_shebang(shebang: &str) -> Option<&'static str> {
    let mut words = shebang.split_whitespace();
    let mut interpreter = words.next()?;
    if interpreter.ends_with("/env") || interpreter == "env" {
        interpreter = words.find(|word| !word.starts_with('-'))?;
    }
    let name = interpreter.rsplit('/').next()?;
    let name = name.trim_end_matches(|c: char| c.is_ascii_digit() || c == '.');
    Some(match name {
        "bash" => "bash",
        "sh" | "dash" | "ash" => "shell",
        "zsh" => "zsh",
        "fish" => "fish",
        "python" => "python",
        "node" | "nodejs" | "deno" | "bun" => "javascript",
        "ruby" => "ruby",
        "perl" => "perl",
        "php" => "php",
        "pwsh" | "powershell" => "powershell",
        "lua" => "lua",
        "Rscript" => "r",
        "awk" | "gawk" => "awk",
        _ => return None,
    })
}

/// The language a file extension implies.
fn language_from_extension(extension: &str) -> Option<&'static str> {
    Some(match extension.to_ascii_lowercase().as_str() {
        "sh" => "shell",
        "bash" => "bash",
        "zsh" => "zsh",
        "fish" => "fish",
        "py" => "python",
        "rb" => "ruby",
        "js" | "mjs" | "cjs" => "javascript",
        "ts" | "mts" => "typescript",
        "pl" => "perl",
        "php" => "php",
        "ps1" => "powershell",
        "sql" => "sql",
        "lua" => "lua",
        "r" => "r",
        "go" => "go",
        "rs" => "rust",
        "awk" => "awk",
        "bat" | "cmd" => "batch",
        _ => return None,
    })
}

/// Infer a reference's media type and canonical format: verified magic bytes
/// first, then the extension, then whether the bytes are UTF-8.
#[must_use]
pub fn media_type_for(package_path: &str, bytes: &[u8]) -> (String, String) {
    if let Some(media) = media_type_from_magic(bytes) {
        return (media.to_owned(), format_for_media_type(media).to_owned());
    }
    let extension = Path::new(package_path)
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase);
    if let Some(media) = extension.as_deref().and_then(media_type_from_extension) {
        return (media.to_owned(), format_for_media_type(media).to_owned());
    }
    if std::str::from_utf8(bytes).is_ok() {
        ("text/plain".to_owned(), "text".to_owned())
    } else {
        ("application/octet-stream".to_owned(), "binary".to_owned())
    }
}

/// A media type proven by the file's leading bytes.
fn media_type_from_magic(bytes: &[u8]) -> Option<&'static str> {
    const SIGNATURES: [(&[u8], &str); 6] = [
        (b"\x89PNG\r\n\x1a\n", "image/png"),
        (b"\xff\xd8\xff", "image/jpeg"),
        (b"GIF87a", "image/gif"),
        (b"GIF89a", "image/gif"),
        (b"%PDF-", "application/pdf"),
        (b"PK\x03\x04", "application/zip"),
    ];
    SIGNATURES
        .iter()
        .find(|(magic, _)| bytes.starts_with(magic))
        .map(|(_, media)| *media)
}

/// A media type implied by the extension.
fn media_type_from_extension(extension: &str) -> Option<&'static str> {
    Some(match extension {
        "md" | "markdown" => "text/markdown",
        "txt" | "text" => "text/plain",
        "json" => "application/json",
        "yaml" | "yml" => "application/yaml",
        "toml" => "application/toml",
        "csv" => "text/csv",
        "html" | "htm" => "text/html",
        "xml" => "application/xml",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "pdf" => "application/pdf",
        "zip" => "application/zip",
        "sh" | "bash" => "text/x-shellscript",
        "py" => "text/x-python",
        "js" | "mjs" => "text/javascript",
        "sql" => "application/sql",
        _ => return None,
    })
}

/// The canonical format of a media type.
fn format_for_media_type(media: &str) -> &'static str {
    match media {
        "text/markdown" => "markdown",
        "text/plain" | "text/x-shellscript" | "text/x-python" | "text/javascript" => "text",
        "application/json" => "json",
        "application/yaml" => "yaml",
        "application/toml" => "toml",
        "text/csv" => "csv",
        "text/html" => "html",
        "application/xml" | "image/svg+xml" => "xml",
        "application/sql" => "sql",
        "application/pdf" => "pdf",
        m if m.starts_with("image/") => "image",
        _ => "binary",
    }
}

// ---------------------------------------------------------------------------
// Projection: typed rows and package edges, inside the sync transaction.
// ---------------------------------------------------------------------------

/// Column-major arrays for the `pgokf.skills` upsert.
#[derive(Default)]
struct SkillColumns {
    concept_ids: Vec<String>,
    visibilities: Vec<String>,
    agent_skills: Vec<String>,
    skill_mds: Vec<Vec<u8>>,
    package_roots: Vec<String>,
    package_hashes: Vec<String>,
    file_hashes: Vec<String>,
}

/// Column-major arrays for the `pgokf.scripts` upsert.
#[derive(Default)]
struct ScriptColumns {
    concept_ids: Vec<String>,
    languages: Vec<String>,
    visibilities: Vec<String>,
    runtimes: Vec<Option<String>>,
    bytes: Vec<Vec<u8>>,
    texts: Vec<String>,
    source_paths: Vec<String>,
    package_ids: Vec<String>,
    file_hashes: Vec<String>,
}

/// Column-major arrays for the `pgokf.reference_documents` upsert.
#[derive(Default)]
struct ReferenceColumns {
    concept_ids: Vec<String>,
    visibilities: Vec<String>,
    formats: Vec<String>,
    media_types: Vec<String>,
    bytes: Vec<Vec<u8>>,
    texts: Vec<Option<String>>,
    source_paths: Vec<String>,
    package_ids: Vec<String>,
    file_hashes: Vec<String>,
}

/// One membership edge from a skill to a resource it owns.
struct MembershipEdge {
    source_id: String,
    target_id: String,
    target_path: String,
    relation: &'static str,
    ordinal: i32,
}

/// Project every staged package member's typed row and the skills' package
/// edges. Runs after the generic rows and the Markdown links are written.
///
/// # Errors
///
/// Returns a [`CatalogError`] on any SPI failure (or an ordinal overflow),
/// aborting the surrounding sync transaction.
pub fn project(
    bundle_id: i64,
    staged: &[StagedConcept],
    text_search_config: &str,
) -> Result<(), CatalogError> {
    let (skills, scripts, references) = collect_columns(staged);
    upsert_skills(bundle_id, &skills)?;
    upsert_scripts(bundle_id, &scripts, text_search_config)?;
    upsert_references(bundle_id, &references, text_search_config)?;
    propagate_visibility(bundle_id, &skills.concept_ids)?;
    let edges = collect_membership_edges(staged)?;
    insert_membership_edges(bundle_id, &edges)?;
    relate_body_links(bundle_id, &skills.concept_ids)?;
    Ok(())
}

fn collect_columns(staged: &[StagedConcept]) -> (SkillColumns, ScriptColumns, ReferenceColumns) {
    let mut skills = SkillColumns::default();
    let mut scripts = ScriptColumns::default();
    let mut references = ReferenceColumns::default();
    for entry in staged {
        match &entry.typed {
            Some(TypedPayload::Skill(payload)) => {
                skills.concept_ids.push(entry.concept.id.clone());
                skills.visibilities.push(payload.visibility.clone());
                skills.agent_skills.push(payload.agent_skill.to_string());
                skills.skill_mds.push(payload.skill_md.clone());
                skills.package_roots.push(payload.package_root.clone());
                skills.package_hashes.push(payload.package_hash.clone());
                skills.file_hashes.push(entry.file_hash.clone());
            }
            Some(TypedPayload::Script(payload)) => {
                scripts.concept_ids.push(entry.concept.id.clone());
                scripts.languages.push(payload.language.clone());
                scripts.visibilities.push(payload.visibility.clone());
                scripts
                    .runtimes
                    .push(payload.runtime.as_ref().map(Value::to_string));
                scripts.bytes.push(payload.bytes.clone());
                scripts.texts.push(entry.concept.body_text.clone());
                scripts.source_paths.push(payload.source_path.clone());
                scripts.package_ids.push(payload.package_concept_id.clone());
                scripts.file_hashes.push(entry.file_hash.clone());
            }
            Some(TypedPayload::Reference(payload)) => {
                references.concept_ids.push(entry.concept.id.clone());
                references.visibilities.push(payload.visibility.clone());
                references.formats.push(payload.format.clone());
                references.media_types.push(payload.media_type.clone());
                references.bytes.push(payload.bytes.clone());
                references.texts.push(payload.text_body.clone());
                references.source_paths.push(payload.source_path.clone());
                references
                    .package_ids
                    .push(payload.package_concept_id.clone());
                references.file_hashes.push(entry.file_hash.clone());
            }
            None => {}
        }
    }
    (skills, scripts, references)
}

fn upsert_skills(bundle_id: i64, columns: &SkillColumns) -> Result<(), CatalogError> {
    const UPSERT: &str = "
        INSERT INTO pgokf.skills
            (bundle_id, tenant_id, concept_id, visibility, agent_skill, skill_md,
             package_root, package_hash, source_file_hash)
        SELECT $1,
               (SELECT b.tenant_id FROM pgokf.bundles b WHERE b.id = $1),
               d.concept_id, d.visibility, d.agent_skill::jsonb, d.skill_md,
               d.package_root, d.package_hash, d.source_file_hash
        FROM unnest($2::text[], $3::text[], $4::text[], $5::bytea[],
                    $6::text[], $7::text[], $8::text[])
             AS d(concept_id, visibility, agent_skill, skill_md,
                  package_root, package_hash, source_file_hash)
        ON CONFLICT (bundle_id, concept_id) DO UPDATE SET
            visibility = excluded.visibility,
            agent_skill = excluded.agent_skill,
            skill_md = excluded.skill_md,
            package_root = excluded.package_root,
            package_hash = excluded.package_hash,
            source_file_hash = excluded.source_file_hash";
    let total = columns.concept_ids.len();
    for start in (0..total).step_by(BATCH_SIZE) {
        let end = usize::min(start + BATCH_SIZE, total);
        Spi::run_with_args(
            UPSERT,
            &[
                bundle_id.into(),
                columns.concept_ids[start..end].to_vec().into(),
                columns.visibilities[start..end].to_vec().into(),
                columns.agent_skills[start..end].to_vec().into(),
                columns.skill_mds[start..end].to_vec().into(),
                columns.package_roots[start..end].to_vec().into(),
                columns.package_hashes[start..end].to_vec().into(),
                columns.file_hashes[start..end].to_vec().into(),
            ],
        )
        .map_err(|error| spi_error("failed to upsert skills", &error))?;
    }
    Ok(())
}

fn upsert_scripts(
    bundle_id: i64,
    columns: &ScriptColumns,
    text_search_config: &str,
) -> Result<(), CatalogError> {
    let upsert = format!(
        "
        INSERT INTO pgokf.scripts
            (bundle_id, tenant_id, concept_id, language, visibility, runtime,
             exact_bytes, byte_size, executable_sha256, source_path,
             package_concept_id, source_file_hash, script_tsv)
        SELECT $1,
               (SELECT b.tenant_id FROM pgokf.bundles b WHERE b.id = $1),
               d.concept_id, d.language, d.visibility, d.runtime::jsonb,
               d.exact_bytes, pg_catalog.length(d.exact_bytes),
               pg_catalog.encode(pg_catalog.sha256(d.exact_bytes), 'hex'),
               d.source_path, d.package_concept_id, d.source_file_hash,
               pg_catalog.to_tsvector($11::pg_catalog.regconfig,
                   pg_catalog.left(d.script_text, {BODY_TSV_CHAR_LIMIT}))
        FROM unnest($2::text[], $3::text[], $4::text[], $5::text[], $6::bytea[],
                    $7::text[], $8::text[], $9::text[], $10::text[])
             AS d(concept_id, language, visibility, runtime, exact_bytes,
                  script_text, source_path, package_concept_id, source_file_hash)
        ON CONFLICT (bundle_id, concept_id) DO UPDATE SET
            language = excluded.language,
            visibility = excluded.visibility,
            runtime = excluded.runtime,
            exact_bytes = excluded.exact_bytes,
            byte_size = excluded.byte_size,
            executable_sha256 = excluded.executable_sha256,
            source_path = excluded.source_path,
            package_concept_id = excluded.package_concept_id,
            source_file_hash = excluded.source_file_hash,
            script_tsv = excluded.script_tsv"
    );
    let total = columns.concept_ids.len();
    for start in (0..total).step_by(BATCH_SIZE) {
        let end = usize::min(start + BATCH_SIZE, total);
        Spi::run_with_args(
            &upsert,
            &[
                bundle_id.into(),
                columns.concept_ids[start..end].to_vec().into(),
                columns.languages[start..end].to_vec().into(),
                columns.visibilities[start..end].to_vec().into(),
                columns.runtimes[start..end].to_vec().into(),
                columns.bytes[start..end].to_vec().into(),
                columns.texts[start..end].to_vec().into(),
                columns.source_paths[start..end].to_vec().into(),
                columns.package_ids[start..end].to_vec().into(),
                columns.file_hashes[start..end].to_vec().into(),
                text_search_config.into(),
            ],
        )
        .map_err(|error| spi_error("failed to upsert scripts", &error))?;
    }
    Ok(())
}

fn upsert_references(
    bundle_id: i64,
    columns: &ReferenceColumns,
    text_search_config: &str,
) -> Result<(), CatalogError> {
    let upsert = format!(
        "
        INSERT INTO pgokf.reference_documents
            (bundle_id, tenant_id, concept_id, visibility, format, media_type,
             exact_bytes, byte_size, content_sha256, text_body, source_path,
             package_concept_id, source_file_hash, reference_tsv)
        SELECT $1,
               (SELECT b.tenant_id FROM pgokf.bundles b WHERE b.id = $1),
               d.concept_id, d.visibility, d.format, d.media_type,
               d.exact_bytes, pg_catalog.length(d.exact_bytes),
               pg_catalog.encode(pg_catalog.sha256(d.exact_bytes), 'hex'),
               d.text_body, d.source_path, d.package_concept_id, d.source_file_hash,
               CASE WHEN d.text_body IS NULL THEN NULL
                    ELSE pg_catalog.to_tsvector($11::pg_catalog.regconfig,
                             pg_catalog.left(d.text_body, {BODY_TSV_CHAR_LIMIT}))
               END
        FROM unnest($2::text[], $3::text[], $4::text[], $5::text[], $6::bytea[],
                    $7::text[], $8::text[], $9::text[], $10::text[])
             AS d(concept_id, visibility, format, media_type, exact_bytes,
                  text_body, source_path, package_concept_id, source_file_hash)
        ON CONFLICT (bundle_id, concept_id) DO UPDATE SET
            visibility = excluded.visibility,
            format = excluded.format,
            media_type = excluded.media_type,
            exact_bytes = excluded.exact_bytes,
            byte_size = excluded.byte_size,
            content_sha256 = excluded.content_sha256,
            text_body = excluded.text_body,
            source_path = excluded.source_path,
            package_concept_id = excluded.package_concept_id,
            source_file_hash = excluded.source_file_hash,
            reference_tsv = excluded.reference_tsv"
    );
    let total = columns.concept_ids.len();
    for start in (0..total).step_by(BATCH_SIZE) {
        let end = usize::min(start + BATCH_SIZE, total);
        Spi::run_with_args(
            &upsert,
            &[
                bundle_id.into(),
                columns.concept_ids[start..end].to_vec().into(),
                columns.visibilities[start..end].to_vec().into(),
                columns.formats[start..end].to_vec().into(),
                columns.media_types[start..end].to_vec().into(),
                columns.bytes[start..end].to_vec().into(),
                columns.texts[start..end].to_vec().into(),
                columns.source_paths[start..end].to_vec().into(),
                columns.package_ids[start..end].to_vec().into(),
                columns.file_hashes[start..end].to_vec().into(),
                text_search_config.into(),
            ],
        )
        .map_err(|error| spi_error("failed to upsert reference documents", &error))?;
    }
    Ok(())
}

/// A member inherits its skill's visibility. Members are re-staged only when
/// their own bytes or ownership change, so when a manifest alone changes its
/// visibility the typed rows and the `visibility` metadata of its members are
/// brought in line here.
fn propagate_visibility(bundle_id: i64, skill_ids: &[String]) -> Result<(), CatalogError> {
    const SCRIPTS: &str = "
        UPDATE pgokf.scripts s SET visibility = k.visibility
        FROM pgokf.skills k
        WHERE s.bundle_id = $1 AND k.bundle_id = $1 AND k.concept_id = ANY($2)
          AND s.package_concept_id = k.concept_id AND s.visibility <> k.visibility";
    const REFERENCES: &str = "
        UPDATE pgokf.reference_documents d SET visibility = k.visibility
        FROM pgokf.skills k
        WHERE d.bundle_id = $1 AND k.bundle_id = $1 AND k.concept_id = ANY($2)
          AND d.package_concept_id = k.concept_id AND d.visibility <> k.visibility";
    const METADATA: &str = "
        UPDATE pgokf.concept_metadata m SET value = pg_catalog.to_jsonb(k.visibility)
        FROM pgokf.skills k,
             (SELECT bundle_id, concept_id, package_concept_id FROM pgokf.scripts
              UNION ALL
              SELECT bundle_id, concept_id, package_concept_id FROM pgokf.reference_documents) r
        WHERE m.bundle_id = $1 AND k.bundle_id = $1 AND r.bundle_id = $1
          AND k.concept_id = ANY($2) AND r.package_concept_id = k.concept_id
          AND m.concept_id = r.concept_id AND m.key = 'visibility'
          AND m.value <> pg_catalog.to_jsonb(k.visibility)";
    for chunk in skill_ids.chunks(BATCH_SIZE) {
        for (statement, what) in [
            (SCRIPTS, "scripts"),
            (REFERENCES, "reference documents"),
            (METADATA, "member metadata"),
        ] {
            Spi::run_with_args(statement, &[bundle_id.into(), chunk.to_vec().into()]).map_err(
                |error| spi_error(&format!("failed to propagate visibility to {what}"), &error),
            )?;
        }
    }
    Ok(())
}

/// One membership edge per owned resource of every staged skill, numbered
/// after the manifest's body links so the `(bundle_id, source_id, ordinal)`
/// key never collides.
fn collect_membership_edges(staged: &[StagedConcept]) -> Result<Vec<MembershipEdge>, CatalogError> {
    let mut edges = Vec::new();
    for entry in staged {
        let Some(TypedPayload::Skill(payload)) = &entry.typed else {
            continue;
        };
        let first = i32::try_from(entry.concept.links.len()).map_err(|_| {
            CatalogError::internal(
                format!(
                    "skill {} has more body links than the i32 ordinal range",
                    entry.concept.id
                ),
                Path::new(""),
            )
        })?;
        for (offset, member) in payload.members.iter().enumerate() {
            let ordinal = i32::try_from(offset)
                .ok()
                .and_then(|o| first.checked_add(o))
                .ok_or_else(|| {
                    CatalogError::internal(
                        format!("skill {} overflows the i32 ordinal range", entry.concept.id),
                        Path::new(""),
                    )
                })?;
            edges.push(MembershipEdge {
                source_id: entry.concept.id.clone(),
                target_id: member.concept_id.clone(),
                target_path: member.path.clone(),
                relation: if member.class == FileClass::SkillScript {
                    USES_RELATION
                } else {
                    REFERENCES_RELATION
                },
                ordinal,
            });
        }
    }
    Ok(edges)
}

/// Insert the membership edges; each resolves against the current concept
/// set exactly as a body link does.
fn insert_membership_edges(bundle_id: i64, edges: &[MembershipEdge]) -> Result<(), CatalogError> {
    const INSERT: &str = "
        INSERT INTO pgokf.links
            (bundle_id, tenant_id, source_id, target_id, link_text, target_path,
             link_kind, resolved, is_external, ordinal, link_relation)
        SELECT
            $1,
            (SELECT b.tenant_id FROM pgokf.bundles b WHERE b.id = $1),
            d.source_id, d.target_id, NULL, d.target_path, $6,
            EXISTS (SELECT 1 FROM pgokf.concepts c
                    WHERE c.bundle_id = $1 AND c.id = d.target_id),
            false, d.ordinal, d.link_relation
        FROM unnest($2::text[], $3::text[], $4::text[], $5::integer[], $7::text[])
             AS d(source_id, target_id, target_path, ordinal, link_relation)";
    let total = edges.len();
    for start in (0..total).step_by(BATCH_SIZE) {
        let chunk = &edges[start..usize::min(start + BATCH_SIZE, total)];
        Spi::run_with_args(
            INSERT,
            &[
                bundle_id.into(),
                chunk
                    .iter()
                    .map(|e| e.source_id.clone())
                    .collect::<Vec<_>>()
                    .into(),
                chunk
                    .iter()
                    .map(|e| e.target_id.clone())
                    .collect::<Vec<_>>()
                    .into(),
                chunk
                    .iter()
                    .map(|e| e.target_path.clone())
                    .collect::<Vec<_>>()
                    .into(),
                chunk.iter().map(|e| e.ordinal).collect::<Vec<_>>().into(),
                MEMBERSHIP_LINK_KIND.into(),
                chunk
                    .iter()
                    .map(|e| e.relation.to_owned())
                    .collect::<Vec<_>>()
                    .into(),
            ],
        )
        .map_err(|error| spi_error("failed to insert package membership edges", &error))?;
    }
    Ok(())
}

/// Give the staged skills' own Markdown links to their resources the same
/// relation as the membership edge (`USES` for a script, `REFERENCES` for a
/// reference or asset), so a reader can select the typed edges either way.
fn relate_body_links(bundle_id: i64, skill_ids: &[String]) -> Result<(), CatalogError> {
    const RELATE: &str = "
        UPDATE pgokf.links l
        SET link_relation = t.relation
        FROM (
            SELECT s.concept_id, s.package_concept_id, $3::text AS relation
            FROM pgokf.scripts s WHERE s.bundle_id = $1
            UNION ALL
            SELECT r.concept_id, r.package_concept_id, $4::text
            FROM pgokf.reference_documents r WHERE r.bundle_id = $1
        ) AS t
        WHERE l.bundle_id = $1
          AND l.source_id = ANY($2)
          AND l.target_id = t.concept_id
          -- Its own resources: a link to another package's script is an
          -- ordinary reference, not this skill's USES edge.
          AND t.package_concept_id = l.source_id
          AND l.link_relation = 'reference'";
    for chunk in skill_ids.chunks(BATCH_SIZE) {
        Spi::run_with_args(
            RELATE,
            &[
                bundle_id.into(),
                chunk.to_vec().into(),
                USES_RELATION.into(),
                REFERENCES_RELATION.into(),
            ],
        )
        .map_err(|error| spi_error("failed to relate skill body links", &error))?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Retrieval: audited, tenant-scoped, exact bytes.
// ---------------------------------------------------------------------------

/// The tenant predicate for `alias.tenant_id`.
fn tenant_filter(alias: &str) -> String {
    format!(
        "(((pg_catalog.current_setting('pgokf.tenant', true) IS NULL
            OR pg_catalog.current_setting('pgokf.tenant', true) = '')
           AND NOT (SELECT pgokf.tenant_required()))
          OR {alias}.tenant_id = pg_catalog.current_setting('pgokf.tenant', true))"
    )
}

/// The `22023` a caller gets for an unknown or untyped concept:
/// identical in every case so it cannot probe another tenant's catalog.
fn not_found(what: &str, bundle_id: i64, concept_id: &str) -> CatalogError {
    CatalogError::invalid_parameter(
        format!("no such {what} {concept_id} in bundle {bundle_id}"),
        Path::new(""),
    )
}

/// A `pgokf.skill_result` row as read from the catalog.
struct SkillRow {
    concept_id: String,
    name: String,
    description: Option<String>,
    package_root: String,
    package_hash: String,
    file_hash: String,
    visibility: String,
    agent_skill: pgrx::JsonB,
    skill_md: Vec<u8>,
}

fn get_skill_impl(
    bundle_id: i64,
    concept_id: &str,
) -> Result<PgHeapTuple<'static, AllocatedByRust>, CatalogError> {
    security::authorize_current_user(security::Operation::Search, Path::new(""))?;
    let query = format!(
        "SELECT s.concept_id, c.title, c.description, s.package_root, s.package_hash,
                s.source_file_hash, s.visibility, s.agent_skill, s.skill_md
         FROM pgokf.skills s
         JOIN pgokf.concepts c ON c.bundle_id = s.bundle_id AND c.id = s.concept_id
         WHERE s.bundle_id = $1 AND s.concept_id = $2 AND {}",
        tenant_filter("s")
    );
    let row: Option<SkillRow> = Spi::connect(|client| {
        let table = client
            .select(&query, Some(1), &[bundle_id.into(), concept_id.into()])
            .map_err(|error| spi_error("failed to read skill", &error))?;
        let Some(first) = table.into_iter().next() else {
            return Ok(None);
        };
        let reader = RowReader::new(&first, "reading a skill", "skill_result");
        Ok(Some(SkillRow {
            concept_id: reader.required(1, "concept_id")?,
            name: reader.required(2, "name")?,
            description: reader.optional(3)?,
            package_root: reader.required(4, "package_root")?,
            package_hash: reader.required(5, "package_hash")?,
            file_hash: reader.required(6, "file_hash")?,
            visibility: reader.required(7, "visibility")?,
            agent_skill: reader.required(8, "agent_skill")?,
            skill_md: reader.required(9, "skill_md")?,
        }))
    })?;
    let row = row.ok_or_else(|| not_found("skill", bundle_id, concept_id))?;
    let resources = skill_resources(bundle_id, concept_id)?;

    crate::catalog::access::record("get_skill", bundle_id, Some(concept_id), None)?;

    let mut tuple = PgHeapTuple::new_composite_type(SKILL_RESULT_TYPE)
        .map_err(|e| composite_error(SKILL_RESULT_TYPE, e))?;
    let err = |e: pgrx::datum::TryFromDatumError| composite_error(SKILL_RESULT_TYPE, e);
    tuple.set_by_name("bundle_id", bundle_id).map_err(err)?;
    tuple
        .set_by_name("concept_id", row.concept_id)
        .map_err(err)?;
    tuple.set_by_name("name", row.name).map_err(err)?;
    tuple
        .set_by_name("description", row.description)
        .map_err(err)?;
    tuple
        .set_by_name("package_root", row.package_root)
        .map_err(err)?;
    tuple
        .set_by_name("package_hash", row.package_hash)
        .map_err(err)?;
    tuple.set_by_name("file_hash", row.file_hash).map_err(err)?;
    tuple
        .set_by_name("visibility", row.visibility)
        .map_err(err)?;
    tuple
        .set_by_name("agent_skill", row.agent_skill)
        .map_err(err)?;
    tuple.set_by_name("skill_md", row.skill_md).map_err(err)?;
    tuple.set_by_name("resources", resources).map_err(err)?;
    Ok(tuple)
}

/// The resources a skill owns, as a JSON array ordered by package path:
/// `{concept_id, class, path, byte_size, sha256, file_hash, language | media_type}`.
fn skill_resources(bundle_id: i64, skill_id: &str) -> Result<pgrx::JsonB, CatalogError> {
    let query = format!(
        "SELECT coalesce(pg_catalog.jsonb_agg(x.r ORDER BY x.r->>'path'), '[]'::jsonb)
         FROM (
             SELECT pg_catalog.jsonb_build_object(
                        'concept_id', s.concept_id, 'class', 'script', 'path', s.source_path,
                        'byte_size', s.byte_size, 'sha256', s.executable_sha256,
                        'file_hash', s.source_file_hash, 'language', s.language) AS r
             FROM pgokf.scripts s
             WHERE s.bundle_id = $1 AND s.package_concept_id = $2 AND {}
             UNION ALL
             SELECT pg_catalog.jsonb_build_object(
                        'concept_id', d.concept_id,
                        'class', CASE WHEN d.source_path LIKE 'assets/%' THEN 'asset' ELSE 'reference' END,
                        'path', d.source_path, 'byte_size', d.byte_size,
                        'sha256', d.content_sha256, 'file_hash', d.source_file_hash,
                        'media_type', d.media_type)
             FROM pgokf.reference_documents d
             WHERE d.bundle_id = $1 AND d.package_concept_id = $2 AND {}
         ) AS x",
        tenant_filter("s"),
        tenant_filter("d")
    );
    Spi::get_one_with_args::<pgrx::JsonB>(&query, &[bundle_id.into(), skill_id.into()])
        .map_err(|error| spi_error("failed to list skill resources", &error))?
        .ok_or_else(|| {
            CatalogError::internal("skill resources aggregate returned NULL", Path::new(""))
        })
}

/// A `pgokf.script_result` row as read from the catalog.
struct ScriptRow {
    concept_id: String,
    title: Option<String>,
    language: String,
    source_path: String,
    package_concept_id: Option<String>,
    byte_size: i64,
    sha256: String,
    runtime: Option<pgrx::JsonB>,
    arguments: Option<pgrx::JsonB>,
    exit_codes: Option<pgrx::JsonB>,
    bytes: Vec<u8>,
}

fn get_script_impl(
    bundle_id: i64,
    concept_id: &str,
) -> Result<PgHeapTuple<'static, AllocatedByRust>, CatalogError> {
    security::authorize_current_user(security::Operation::Search, Path::new(""))?;
    let query = format!(
        "SELECT s.concept_id, c.title, s.language, s.source_path, s.package_concept_id,
                s.byte_size, s.executable_sha256, s.runtime, s.arguments, s.exit_codes,
                s.exact_bytes
         FROM pgokf.scripts s
         JOIN pgokf.concepts c ON c.bundle_id = s.bundle_id AND c.id = s.concept_id
         WHERE s.bundle_id = $1 AND s.concept_id = $2 AND {}",
        tenant_filter("s")
    );
    let row: Option<ScriptRow> = Spi::connect(|client| {
        let table = client
            .select(&query, Some(1), &[bundle_id.into(), concept_id.into()])
            .map_err(|error| spi_error("failed to read script", &error))?;
        let Some(first) = table.into_iter().next() else {
            return Ok(None);
        };
        let reader = RowReader::new(&first, "reading a script", "script_result");
        Ok(Some(ScriptRow {
            concept_id: reader.required(1, "concept_id")?,
            title: reader.optional(2)?,
            language: reader.required(3, "language")?,
            source_path: reader.required(4, "source_path")?,
            package_concept_id: reader.optional(5)?,
            byte_size: reader.required(6, "byte_size")?,
            sha256: reader.required(7, "executable_sha256")?,
            runtime: reader.optional(8)?,
            arguments: reader.optional(9)?,
            exit_codes: reader.optional(10)?,
            bytes: reader.required(11, "exact_bytes")?,
        }))
    })?;
    let row = row.ok_or_else(|| not_found("script", bundle_id, concept_id))?;

    crate::catalog::access::record("get_script", bundle_id, Some(concept_id), None)?;

    let mut tuple = PgHeapTuple::new_composite_type(SCRIPT_RESULT_TYPE)
        .map_err(|e| composite_error(SCRIPT_RESULT_TYPE, e))?;
    let err = |e: pgrx::datum::TryFromDatumError| composite_error(SCRIPT_RESULT_TYPE, e);
    tuple.set_by_name("bundle_id", bundle_id).map_err(err)?;
    tuple
        .set_by_name("concept_id", row.concept_id)
        .map_err(err)?;
    tuple.set_by_name("title", row.title).map_err(err)?;
    tuple.set_by_name("language", row.language).map_err(err)?;
    tuple
        .set_by_name("source_path", row.source_path)
        .map_err(err)?;
    tuple
        .set_by_name("package_concept_id", row.package_concept_id)
        .map_err(err)?;
    tuple.set_by_name("byte_size", row.byte_size).map_err(err)?;
    tuple
        .set_by_name("executable_sha256", row.sha256)
        .map_err(err)?;
    tuple.set_by_name("runtime", row.runtime).map_err(err)?;
    tuple.set_by_name("arguments", row.arguments).map_err(err)?;
    tuple
        .set_by_name("exit_codes", row.exit_codes)
        .map_err(err)?;
    tuple.set_by_name("exact_bytes", row.bytes).map_err(err)?;
    Ok(tuple)
}

/// A `pgokf.reference_result` row as read from the catalog.
struct ReferenceRow {
    concept_id: String,
    title: Option<String>,
    format: String,
    media_type: String,
    source_path: String,
    package_concept_id: Option<String>,
    byte_size: i64,
    sha256: String,
    text_body: Option<String>,
    bytes: Option<Vec<u8>>,
}

fn get_reference_impl(
    bundle_id: i64,
    concept_id: &str,
    include_bytes: bool,
) -> Result<PgHeapTuple<'static, AllocatedByRust>, CatalogError> {
    security::authorize_current_user(security::Operation::Search, Path::new(""))?;
    let query = format!(
        "SELECT d.concept_id, c.title, d.format, d.media_type, d.source_path,
                d.package_concept_id, d.byte_size, d.content_sha256, d.text_body,
                CASE WHEN $3 THEN d.exact_bytes ELSE NULL END
         FROM pgokf.reference_documents d
         JOIN pgokf.concepts c ON c.bundle_id = d.bundle_id AND c.id = d.concept_id
         WHERE d.bundle_id = $1 AND d.concept_id = $2 AND {}",
        tenant_filter("d")
    );
    let row: Option<ReferenceRow> = Spi::connect(|client| {
        let table = client
            .select(
                &query,
                Some(1),
                &[bundle_id.into(), concept_id.into(), include_bytes.into()],
            )
            .map_err(|error| spi_error("failed to read reference", &error))?;
        let Some(first) = table.into_iter().next() else {
            return Ok(None);
        };
        let reader = RowReader::new(&first, "reading a reference", "reference_result");
        Ok(Some(ReferenceRow {
            concept_id: reader.required(1, "concept_id")?,
            title: reader.optional(2)?,
            format: reader.required(3, "format")?,
            media_type: reader.required(4, "media_type")?,
            source_path: reader.required(5, "source_path")?,
            package_concept_id: reader.optional(6)?,
            byte_size: reader.required(7, "byte_size")?,
            sha256: reader.required(8, "content_sha256")?,
            text_body: reader.optional(9)?,
            bytes: reader.optional(10)?,
        }))
    })?;
    let row = row.ok_or_else(|| not_found("reference", bundle_id, concept_id))?;

    // Audited whether or not the bytes are returned: a textual reference's
    // `text_body` is its complete content.
    crate::catalog::access::record(
        "get_reference",
        bundle_id,
        Some(concept_id),
        (!include_bytes).then_some("metadata"),
    )?;

    let mut tuple = PgHeapTuple::new_composite_type(REFERENCE_RESULT_TYPE)
        .map_err(|e| composite_error(REFERENCE_RESULT_TYPE, e))?;
    let err = |e: pgrx::datum::TryFromDatumError| composite_error(REFERENCE_RESULT_TYPE, e);
    tuple.set_by_name("bundle_id", bundle_id).map_err(err)?;
    tuple
        .set_by_name("concept_id", row.concept_id)
        .map_err(err)?;
    tuple.set_by_name("title", row.title).map_err(err)?;
    tuple.set_by_name("format", row.format).map_err(err)?;
    tuple
        .set_by_name("media_type", row.media_type)
        .map_err(err)?;
    tuple
        .set_by_name("source_path", row.source_path)
        .map_err(err)?;
    tuple
        .set_by_name("package_concept_id", row.package_concept_id)
        .map_err(err)?;
    tuple.set_by_name("byte_size", row.byte_size).map_err(err)?;
    tuple
        .set_by_name("content_sha256", row.sha256)
        .map_err(err)?;
    tuple.set_by_name("text_body", row.text_body).map_err(err)?;
    tuple.set_by_name("exact_bytes", row.bytes).map_err(err)?;
    Ok(tuple)
}

/// SQL-facing package retrieval entry points, installed into the `pgokf` schema.
#[pgrx::pg_schema]
mod pgokf {
    use pgrx::{default, extension_sql, pg_extern};

    use super::{get_reference_impl, get_script_impl, get_skill_impl};

    extension_sql!(
        r"
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
",
        name = "package_result_types",
        requires = ["package_tables"]
    );

    /// Read one Agent Skills package: metadata, exact `SKILL.md` bytes, and
    /// its resource listing.
    ///
    /// Requires membership in `pgokf_reader`. Tenant-scoped: a scoped session
    /// gets the same `22023` for another tenant's skill as for an unknown
    /// one. Every successful read appends a `get_skill` row to the access log.
    #[pg_extern(stable, requires = ["package_result_types", "package_tables"])]
    fn get_skill(
        bundle_id: i64,
        concept_id: &str,
    ) -> pgrx::composite_type!('static, "pgokf.skill_result") {
        get_skill_impl(bundle_id, concept_id).unwrap_or_else(|error| error.raise())
    }

    /// Read one package script's exact bytes and typed metadata.
    ///
    /// Requires membership in `pgokf_reader`; tenant-scoped and audited
    /// (`get_script`) like `get_concept_source`. Never returns `body_text`.
    #[pg_extern(stable, requires = ["package_result_types", "package_tables"])]
    fn get_script(
        bundle_id: i64,
        concept_id: &str,
    ) -> pgrx::composite_type!('static, "pgokf.script_result") {
        get_script_impl(bundle_id, concept_id).unwrap_or_else(|error| error.raise())
    }

    /// Read one package reference or asset: metadata, its text when textual,
    /// and (by default) its exact bytes.
    ///
    /// Requires membership in `pgokf_reader`; tenant-scoped and audited
    /// (`get_reference`) like `get_concept_source`; a read with
    /// `include_bytes => false` is logged with the detail `metadata`.
    #[pg_extern(stable, requires = ["package_result_types", "package_tables"])]
    fn get_reference(
        bundle_id: i64,
        concept_id: &str,
        include_bytes: default!(bool, true),
    ) -> pgrx::composite_type!('static, "pgokf.reference_result") {
        get_reference_impl(bundle_id, concept_id, include_bytes)
            .unwrap_or_else(|error| error.raise())
    }

    extension_sql!(
        r"
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
",
        name = "package_function_hardening",
        requires = [get_skill, get_script, get_reference]
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn entry(path: &str, class: FileClass, hash: &str) -> FileMetadata {
        FileMetadata {
            path: PathBuf::from(path),
            hash: hash.to_owned(),
            size_bytes: 1,
            modified_at: None,
            class,
        }
    }

    fn context<'a>(path: &'a str, package_path: &'a str) -> ResourceContext<'a> {
        ResourceContext {
            path,
            package_path,
            package_concept_id: "pkg/SKILL",
            visibility: "internal",
            file_hash: "hash",
            modified_at_epoch: Some(1.0),
        }
    }

    #[test]
    fn package_hash_is_order_independent_and_member_sensitive() {
        // Arrange
        let a = PackageMember {
            class: FileClass::SkillScript,
            path: "pkg/scripts/a.sh".into(),
            package_path: "scripts/a.sh".into(),
            concept_id: "pkg/scripts/a.sh".into(),
            file_hash: "h1".into(),
        };
        let b = PackageMember {
            class: FileClass::SkillAsset,
            path: "pkg/assets/b.png".into(),
            package_path: "assets/b.png".into(),
            concept_id: "pkg/assets/b.png".into(),
            file_hash: "h2".into(),
        };

        // Act
        let forward = package_hash("m", &[a.clone(), b.clone()]);
        let backward = package_hash("m", &[b.clone(), a.clone()]);
        let changed = package_hash(
            "m",
            &[
                PackageMember {
                    file_hash: "h9".into(),
                    ..a.clone()
                },
                b.clone(),
            ],
        );
        let fewer = package_hash("m", &[a]);

        // Assert
        assert_eq!(forward, backward);
        assert_ne!(forward, changed);
        assert_ne!(forward, fewer);
        assert_ne!(forward, package_hash("other", &[b]));
    }

    #[test]
    fn invalidate_packages_reschedules_a_manifest_whose_member_changed() {
        // Arrange: the manifest is unchanged, one script changed.
        let current = vec![
            entry("pkg/SKILL.md", FileClass::SkillManifest, "m"),
            entry("pkg/scripts/run.sh", FileClass::SkillScript, "s2"),
            entry("docs/a.md", FileClass::OkfDocument, "d"),
        ];
        let mut to_parse = vec![entry("pkg/scripts/run.sh", FileClass::SkillScript, "s2")];

        // Act
        invalidate_packages(&mut to_parse, &[], &current, &BTreeMap::new());

        // Assert
        let paths: Vec<String> = to_parse
            .iter()
            .map(|e| e.path.to_string_lossy().into_owned())
            .collect();
        assert_eq!(paths, ["pkg/scripts/run.sh", "pkg/SKILL.md"]);
    }

    #[test]
    fn invalidate_packages_reschedules_on_a_removed_member_only() {
        // Arrange
        let current = vec![
            entry("pkg/SKILL.md", FileClass::SkillManifest, "m"),
            entry("other/SKILL.md", FileClass::SkillManifest, "o"),
        ];
        let mut to_parse = Vec::new();

        // Act: a removed script of pkg and a removed plain document of other.
        invalidate_packages(
            &mut to_parse,
            &[
                "pkg/scripts/gone.sh".to_owned(),
                "other/notes.md".to_owned(),
            ],
            &current,
            &BTreeMap::new(),
        );

        // Assert
        let paths: Vec<String> = to_parse
            .iter()
            .map(|e| e.path.to_string_lossy().into_owned())
            .collect();
        assert_eq!(paths, ["pkg/SKILL.md"]);
    }

    #[test]
    fn invalidate_packages_does_not_duplicate_a_scheduled_manifest() {
        // Arrange
        let current = vec![
            entry("pkg/SKILL.md", FileClass::SkillManifest, "m"),
            entry("pkg/assets/a.png", FileClass::SkillAsset, "a"),
        ];
        let mut to_parse = vec![
            entry("pkg/SKILL.md", FileClass::SkillManifest, "m"),
            entry("pkg/assets/a.png", FileClass::SkillAsset, "a"),
        ];

        // Act
        invalidate_packages(&mut to_parse, &[], &current, &BTreeMap::new());

        // Assert
        assert_eq!(to_parse.len(), 2);
    }

    #[test]
    fn package_members_lists_owned_resources_in_path_order() {
        // Arrange
        let current = vec![
            entry("pkg/SKILL.md", FileClass::SkillManifest, "m"),
            entry("pkg/scripts/z.sh", FileClass::SkillScript, "z"),
            entry("pkg/assets/a.png", FileClass::SkillAsset, "a"),
            entry("other/SKILL.md", FileClass::SkillManifest, "o"),
            entry("other/scripts/x.sh", FileClass::SkillScript, "x"),
        ];
        let index = PackageIndex::from_paths(["pkg/SKILL.md", "other/SKILL.md"]);

        // Act
        let members = package_members("pkg", &index, &current);

        // Assert
        let ids: Vec<&str> = members.iter().map(|m| m.concept_id.as_str()).collect();
        assert_eq!(ids, ["pkg/assets/a.png", "pkg/scripts/z.sh"]);
        assert_eq!(members[1].package_path, "scripts/z.sh");
        assert_eq!(members[1].class, FileClass::SkillScript);
    }

    #[test]
    fn stage_manifest_carries_package_identity_and_findings() {
        // Arrange
        let source = b"---\nname: Deploy_Now\ndescription: d\nvisibility: private\n---\n# Use\n[run](scripts/run.sh)\n";
        let members = vec![PackageMember {
            class: FileClass::SkillScript,
            path: "pkg/scripts/run.sh".into(),
            package_path: "scripts/run.sh".into(),
            concept_id: "pkg/scripts/run.sh".into(),
            file_hash: "s".into(),
        }];

        // Act
        let (staged, findings) = stage_manifest(
            source.to_vec(),
            Path::new("pkg/SKILL.md"),
            "m",
            None,
            members,
            ParserLimits::default(),
            false,
        )
        .expect("manifest stages");

        // Assert
        let Some(TypedPayload::Skill(payload)) = &staged.typed else {
            panic!("a manifest stages a skill payload");
        };
        assert_eq!(staged.concept.id, "pkg/SKILL");
        assert_eq!(payload.package_root, "pkg");
        assert_eq!(payload.visibility, "private");
        assert_eq!(payload.skill_md, source);
        assert_eq!(payload.agent_skill["name"], "Deploy_Now");
        assert_eq!(payload.members.len(), 1);
        assert!(staged.raw_content.is_none());
        assert_eq!(
            findings.len(),
            2,
            "invalid name and directory mismatch: {findings:?}"
        );
        assert_eq!(
            staged.concept.links[0].target_id.as_deref(),
            Some("pkg/scripts/run.sh")
        );
    }

    #[test]
    fn stage_script_infers_language_from_shebang_before_extension() {
        // Arrange
        let bytes = b"#!/usr/bin/env python3\n# Checks replication lag.\nprint(1)\n".to_vec();

        // Act
        let staged = stage_script(
            bytes.clone(),
            &context("pkg/scripts/check.sh", "scripts/check.sh"),
            true,
        )
        .expect("utf-8 script stages");

        // Assert
        let Some(TypedPayload::Script(payload)) = &staged.typed else {
            panic!("a script stages a script payload");
        };
        assert_eq!(staged.concept.id, "pkg/scripts/check.sh");
        assert_eq!(staged.concept.r#type, "Script");
        assert_eq!(staged.concept.title, "check.sh");
        assert_eq!(
            staged.concept.description.as_deref(),
            Some("Checks replication lag.")
        );
        assert_eq!(payload.language, "python");
        assert_eq!(
            payload
                .runtime
                .as_ref()
                .and_then(|r| r["executable"].as_str()),
            Some("/usr/bin/env python3")
        );
        assert_eq!(staged.concept.metadata["package"], "pkg/SKILL");
        assert_eq!(staged.concept.metadata["resource_class"], "script");
        assert_eq!(staged.raw_content.as_deref(), Some(bytes.as_slice()));
    }

    #[test]
    fn stage_script_rejects_bytes_it_could_not_store_as_text() {
        // Arrange: bytes that are not UTF-8 at all, and bytes that are
        // valid UTF-8 but hold U+0000 - which PostgreSQL's `text` cannot
        // represent, so storing one would leave the bytes and the text
        // disagreeing about the file.
        let binary = vec![0xff, 0xfe, 0x00, 0x41];
        let with_nul = b"#!/bin/sh\0echo\n".to_vec();

        // Act
        let from_binary = stage_script(binary, &context("pkg/scripts/tool", "scripts/tool"), false)
            .expect_err("binary is not a script");
        let from_nul = stage_script(
            with_nul,
            &context("pkg/scripts/tool", "scripts/tool"),
            false,
        )
        .expect_err("a NUL is not text");

        // Assert
        for error in [from_binary, from_nul] {
            assert!(
                error
                    .message()
                    .contains("not text a script can be stored as"),
                "{}",
                error.message()
            );
        }
    }

    #[test]
    fn infer_language_falls_back_to_the_extension_then_unknown() {
        // Arrange / Act / Assert
        assert_eq!(infer_language("scripts/a.rb", "puts 1").0, "ruby");
        assert_eq!(infer_language("scripts/a", "echo").0, "unknown");
        assert_eq!(infer_language("scripts/a.sh", "#!/bin/sh\n").0, "shell");
        assert_eq!(
            infer_language("scripts/a.txt", "#!/usr/bin/env -S node\n").0,
            "javascript"
        );
        assert!(infer_language("scripts/a.sh", "echo").1.is_none());
    }

    #[test]
    fn stage_reference_reads_markdown_headings_and_keeps_text() {
        // Arrange
        let bytes = b"# Failover guide\n\nStep one.\n".to_vec();

        // Act
        let staged = stage_reference(
            bytes.clone(),
            &context("pkg/references/guide.md", "references/guide.md"),
            false,
        );

        // Assert
        let Some(TypedPayload::Reference(payload)) = &staged.typed else {
            panic!("a reference stages a reference payload");
        };
        assert_eq!(staged.concept.id, "pkg/references/guide.md");
        assert_eq!(staged.concept.r#type, "Reference");
        assert_eq!(staged.concept.title, "Failover guide");
        assert_eq!(staged.concept.body_text.trim(), "Failover guide\nStep one.");
        assert_eq!(payload.format, "markdown");
        assert_eq!(payload.media_type, "text/markdown");
        assert_eq!(
            payload.text_body.as_deref(),
            Some("# Failover guide\n\nStep one.\n")
        );
        assert_eq!(staged.concept.metadata["resource_class"], "reference");
    }

    #[test]
    fn stage_reference_treats_a_png_as_a_binary_asset() {
        // Arrange: PNG magic bytes with a misleading extension.
        let bytes = b"\x89PNG\r\n\x1a\n\x00\x00".to_vec();

        // Act
        let staged = stage_reference(
            bytes,
            &context("pkg/assets/diagram.txt", "assets/diagram.txt"),
            false,
        );

        // Assert
        let Some(TypedPayload::Reference(payload)) = &staged.typed else {
            panic!("an asset stages a reference payload");
        };
        assert_eq!(payload.media_type, "image/png");
        assert_eq!(payload.format, "image");
        assert!(payload.text_body.is_none());
        assert_eq!(staged.concept.title, "diagram.txt");
        assert_eq!(staged.concept.body_text, "");
        assert_eq!(staged.concept.metadata["resource_class"], "asset");
    }

    #[test]
    fn media_type_for_uses_extension_then_utf8_sniffing() {
        // Arrange / Act / Assert
        assert_eq!(
            media_type_for("references/a.json", b"{}"),
            ("application/json".to_owned(), "json".to_owned())
        );
        assert_eq!(
            media_type_for("references/notes", b"plain words"),
            ("text/plain".to_owned(), "text".to_owned())
        );
        assert_eq!(
            media_type_for("assets/blob", b"\x00\xff\xfe"),
            ("application/octet-stream".to_owned(), "binary".to_owned())
        );
        assert_eq!(
            media_type_for("assets/doc.pdf", b"%PDF-1.7"),
            ("application/pdf".to_owned(), "pdf".to_owned())
        );
    }

    #[test]
    fn skill_visibility_accepts_only_the_three_values() {
        // Arrange / Act / Assert
        let json = |text: &str| text.parse::<Value>().expect("fixture JSON parses");
        assert_eq!(
            skill_visibility(&json(r#"{"visibility": "public"}"#)),
            "public"
        );
        assert_eq!(
            skill_visibility(&json(r#"{"visibility": "secret"}"#)),
            "internal"
        );
        assert_eq!(skill_visibility(&json("{}")), "internal");
    }

    #[test]
    fn collect_membership_edges_numbers_after_body_links_and_picks_relations() {
        // Arrange
        let source =
            b"---\nname: pkg\ndescription: d\n---\n[a](scripts/a.sh) [b](https://x.invalid)\n";
        let members = vec![
            PackageMember {
                class: FileClass::SkillScript,
                path: "pkg/scripts/a.sh".into(),
                package_path: "scripts/a.sh".into(),
                concept_id: "pkg/scripts/a.sh".into(),
                file_hash: "a".into(),
            },
            PackageMember {
                class: FileClass::SkillReference,
                path: "pkg/references/r.md".into(),
                package_path: "references/r.md".into(),
                concept_id: "pkg/references/r.md".into(),
                file_hash: "r".into(),
            },
        ];
        let (staged, _) = stage_manifest(
            source.to_vec(),
            Path::new("pkg/SKILL.md"),
            "m",
            None,
            members,
            ParserLimits::default(),
            false,
        )
        .expect("manifest stages");

        // Act
        let edges = collect_membership_edges(std::slice::from_ref(&staged)).expect("edges collect");

        // Assert
        let summary: Vec<(&str, &str, i32)> = edges
            .iter()
            .map(|e| (e.target_id.as_str(), e.relation, e.ordinal))
            .collect();
        assert_eq!(
            summary,
            [
                ("pkg/scripts/a.sh", "USES", 2),
                ("pkg/references/r.md", "REFERENCES", 3)
            ]
        );
    }

    #[test]
    fn tenant_filter_scopes_the_aliased_table() {
        // Arrange / Act / Assert
        assert!(tenant_filter("s").contains("s.tenant_id"));
        assert!(tenant_filter("s").contains("pgokf.tenant_required()"));
    }

    #[test]
    fn a_nested_manifest_touches_the_enclosing_package() {
        // Arrange: `a/scripts/inner/SKILL.md` was just added; its script used
        // to belong to `a` and is unchanged as a file.
        let current = vec![
            entry("a/SKILL.md", FileClass::SkillManifest, "m"),
            entry("a/scripts/inner/SKILL.md", FileClass::SkillManifest, "n"),
            entry("a/scripts/inner/scripts/x.sh", FileClass::SkillScript, "x"),
        ];
        let mut to_parse = vec![entry(
            "a/scripts/inner/SKILL.md",
            FileClass::SkillManifest,
            "n",
        )];

        // Act
        invalidate_packages(&mut to_parse, &[], &current, &BTreeMap::new());

        // Assert: the outer manifest is re-staged as well.
        let paths: Vec<String> = to_parse
            .iter()
            .map(|e| e.path.to_string_lossy().into_owned())
            .collect();
        assert_eq!(paths, ["a/scripts/inner/SKILL.md", "a/SKILL.md"]);
    }

    #[test]
    fn a_removed_nested_manifest_and_a_previous_owner_are_touched() {
        // Arrange: `a/scripts/inner/SKILL.md` was removed; a file that lived
        // directly under the nested root stopped being content (removed
        // path, previously owned by the nested skill), and nothing else
        // changed.
        let current = vec![
            entry("a/SKILL.md", FileClass::SkillManifest, "m"),
            entry("a/scripts/inner/scripts/x.sh", FileClass::SkillScript, "x"),
        ];
        let removed = vec![
            "a/scripts/inner/SKILL.md".to_owned(),
            "a/scripts/inner/tool".to_owned(),
        ];
        let owners = BTreeMap::from([(
            "a/scripts/inner/tool".to_owned(),
            "a/scripts/inner/SKILL".to_owned(),
        )]);
        let mut to_parse = Vec::new();

        // Act
        invalidate_packages(&mut to_parse, &removed, &current, &owners);

        // Assert: the enclosing package is re-staged (the removed nested
        // package no longer exists, so there is nothing else to stage).
        let paths: Vec<String> = to_parse
            .iter()
            .map(|e| e.path.to_string_lossy().into_owned())
            .collect();
        assert_eq!(paths, ["a/SKILL.md"]);
    }

    #[test]
    fn package_root_and_skill_id_round_trip() {
        // Arrange / Act / Assert
        assert_eq!(package_root_of("skills/deploy/SKILL"), "skills/deploy");
        assert_eq!(package_root_of("SKILL"), "");
        assert_eq!(skill_concept_id_of("skills/deploy"), "skills/deploy/SKILL");
        assert_eq!(skill_concept_id_of(""), "SKILL");
    }

    #[test]
    fn first_heading_skips_fenced_code_and_stage_reference_drops_frontmatter() {
        // Arrange
        let text = "```sh\n# not a heading\n```\n\n# Real heading\n";
        let with_frontmatter =
            b"---\ntype: Reference\ntitle: Ignored\n---\n\n# From the body\n\nWords.\n".to_vec();

        // Act
        let heading = first_heading(text);
        let staged = stage_reference(
            with_frontmatter,
            &context("pkg/references/g.md", "references/g.md"),
            false,
        );

        // Assert
        assert_eq!(heading.as_deref(), Some("Real heading"));
        assert_eq!(staged.concept.title, "From the body");
        assert!(!staged.concept.body_text.contains("type: Reference"));
        assert!(staged.concept.body_text.contains("Words."));
    }

    #[test]
    fn an_svg_reference_keeps_its_text() {
        // Arrange
        let bytes = b"<svg xmlns=\"http://www.w3.org/2000/svg\"/>\n".to_vec();

        // Act
        let staged = stage_reference(bytes, &context("pkg/assets/d.svg", "assets/d.svg"), false);

        // Assert
        let Some(TypedPayload::Reference(payload)) = &staged.typed else {
            panic!("a reference payload");
        };
        assert_eq!(payload.media_type, "image/svg+xml");
        assert_eq!(payload.format, "xml");
        assert!(payload.text_body.is_some());
    }
}
