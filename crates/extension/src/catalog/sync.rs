//! The shared register/refresh engine.
//!
//! `pgokf.register_bundle` and `pgokf.refresh_bundle` both funnel into
//! [`run_bundle_sync`], which is a single-transaction, set-based diff:
//!
//! 1. serialize on a bundle-scoped advisory lock ([`advisory_lock_key`]);
//! 2. load the stored `path -> file_hash` projection for the bundle;
//! 3. discover the current filesystem state via [`okf_sync::discover`]
//!    (symlink-escape safe, size/count limited from the `pgokf.*` GUCs);
//! 4. classify changes against the stored hashes ([`classify_changes`]) so
//!    unchanged rows are never rewritten and `indexed_at` is preserved;
//! 5. parse only added/updated files with [`okf_parser::parse_concept`];
//! 6. delete removed rows, upsert changed rows (recomputing the weighted
//!    `body_tsv`), and replace `concept_metadata` for touched concepts;
//! 7. run the ordered projection seam — [`crate::catalog::links::project`],
//!    then [`crate::catalog::provenance::project`] — with the staged
//!    concepts;
//! 8. update the bundle row (file count, `last_synced_at`, aggregate
//!    `sync_hash`) last.
//!
//! Because `pgrx` functions execute inside the caller's transaction and every
//! failure is raised as a `PostgreSQL` error, a failed sync rolls back
//! atomically; strict per-file parse failures therefore never commit a
//! partial projection.
//!
//! # Path containment
//!
//! Bundle roots are validated with [`crate::security::validate_path_syntax`]
//! (absolute, no NUL, no `..`) and canonicalized before use, and
//! [`okf_sync::discover`] rejects symlinks that escape the canonical root.
//! Allowed-roots containment ([`crate::security::canonicalize_contained_path`])
//! is intentionally not enforced yet: the configuration surface that stores
//! `allowed_roots` lands with [`crate::catalog::config`], which will tighten
//! registration to configured roots without touching this engine. Until
//! then, any absolute, canonical, traversal-free path is accepted — and
//! registration remains restricted to `pgokf_admin`.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use okf_parser::{ParserLimits, is_reserved_path, parse_concept};
use okf_sync::{FileMetadata, Snapshot, SyncConfig, SyncReport, discover, hash_bytes};
use pgrx::Spi;

use crate::catalog::batch::{self, BATCH_SIZE};
use crate::catalog::types::StagedConcept;
use crate::errors::CatalogError;
use crate::guc;
use crate::security;

/// Namespace prefix mixed into every advisory-lock key so `pgokf` locks
/// cannot collide with another subsystem hashing similar strings.
const ADVISORY_LOCK_NAMESPACE: &str = "pgokf.bundle";

/// Derive the stable, bundle-scoped `pg_advisory_xact_lock` key for a
/// canonical bundle path.
///
/// The key is the first 8 bytes of the BLAKE3 digest of
/// `"pgokf.bundle:<path>"`, reinterpreted as a signed 64-bit integer. Both
/// `register_bundle` and `refresh_bundle` lock this key before touching
/// catalog state, so concurrent syncs of one bundle serialize while distinct
/// bundles proceed in parallel.
///
/// # Panics
///
/// Never panics in practice: a BLAKE3 hex digest is always 64 lowercase hex
/// characters, so the slice and radix parse cannot fail.
#[must_use]
pub fn advisory_lock_key(canonical_path: &str) -> i64 {
    let digest = hash_bytes(format!("{ADVISORY_LOCK_NAMESPACE}:{canonical_path}").as_bytes());
    let leading = u64::from_str_radix(&digest[..16], 16)
        .expect("a BLAKE3 hex digest always begins with 16 hexadecimal characters");
    i64::from_le_bytes(leading.to_le_bytes())
}

/// The classified difference between the stored catalog projection and the
/// current filesystem snapshot of one bundle.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BundleDelta {
    /// Files that must be (re-)parsed and upserted: added or content-changed.
    pub to_parse: Vec<FileMetadata>,
    /// Stored concept paths whose files no longer exist on disk.
    pub removed_paths: Vec<String>,
    /// Count-only summary of the classification.
    pub report: SyncReport,
}

/// Classify the current snapshot against the stored `path -> file_hash`
/// projection.
///
/// Reserved OKF files (`index.md` / `log.md`) are never concepts: they are
/// skipped entirely and count toward no bucket. A file whose stored BLAKE3
/// hash matches its current hash is unchanged and will not be rewritten.
#[must_use]
pub fn classify_changes(stored: &BTreeMap<String, String>, current: &Snapshot) -> BundleDelta {
    let mut delta = BundleDelta::default();
    let mut current_paths = BTreeSet::new();

    for (path, metadata) in current.files() {
        let path_text = path.to_string_lossy().into_owned();
        if is_reserved_path(&path_text) {
            continue;
        }
        match stored.get(&path_text) {
            None => {
                delta.report.added += 1;
                delta.to_parse.push(metadata.clone());
            }
            Some(stored_hash) if *stored_hash == metadata.hash => {
                delta.report.unchanged += 1;
            }
            Some(_) => {
                delta.report.updated += 1;
                delta.to_parse.push(metadata.clone());
            }
        }
        current_paths.insert(path_text);
    }

    for path in stored.keys() {
        if !current_paths.contains(path) {
            delta.report.removed += 1;
            delta.removed_paths.push(path.clone());
        }
    }

    delta
}

/// Aggregate BLAKE3 digest over the sorted `(path, file_hash)` pairs of a
/// snapshot, excluding reserved OKF files.
///
/// Stored on `pgokf.bundles.sync_hash` after every successful sync so
/// operators (and later waves) can detect drift without re-hashing files.
#[must_use]
pub fn bundle_sync_hash(current: &Snapshot) -> String {
    let mut buffer = String::new();
    for (path, metadata) in current.files() {
        let path_text = path.to_string_lossy();
        if is_reserved_path(&path_text) {
            continue;
        }
        buffer.push_str(&path_text);
        buffer.push('\0');
        buffer.push_str(&metadata.hash);
        buffer.push('\n');
    }
    hash_bytes(buffer.as_bytes())
}

fn spi_error(context: &str, error: &pgrx::spi::Error) -> CatalogError {
    CatalogError::internal(format!("{context}: {error}"), Path::new(""))
}

/// Resolve and canonicalize a caller-supplied bundle root.
///
/// Enforces [`security::validate_path_syntax`] (absolute path, no NUL bytes,
/// no `..` components) and then canonicalizes, so the value stored on
/// `pgokf.bundles.path` — and used for advisory-lock keying — is always the
/// resolved filesystem path. See the module docs for why allowed-roots
/// containment is deferred to the config wave.
fn resolve_bundle_root(path: &str) -> Result<PathBuf, CatalogError> {
    let requested = Path::new(path);
    security::validate_path_syntax(requested, Path::new(""))?;
    let canonical = std::fs::canonicalize(requested).map_err(|error| {
        CatalogError::invalid_parameter(
            format!("failed to canonicalize bundle path {path}: {error}"),
            Path::new(""),
        )
    })?;
    if !canonical.is_dir() {
        return Err(CatalogError::invalid_parameter(
            format!("bundle path is not a directory: {path}"),
            Path::new(""),
        ));
    }
    Ok(canonical)
}

fn canonical_path_text(canonical: &Path) -> Result<&str, CatalogError> {
    canonical.to_str().ok_or_else(|| {
        CatalogError::invalid_parameter(
            format!(
                "canonical bundle path is not valid UTF-8: {}",
                canonical.display()
            ),
            Path::new(""),
        )
    })
}

fn acquire_bundle_lock(canonical_path: &str) -> Result<(), CatalogError> {
    let key = advisory_lock_key(canonical_path);
    Spi::run_with_args("SELECT pg_catalog.pg_advisory_xact_lock($1)", &[key.into()])
        .map_err(|error| spi_error("failed to acquire bundle advisory lock", &error))
}

fn load_stored_hashes(bundle_id: i64) -> Result<BTreeMap<String, String>, CatalogError> {
    Spi::connect(|client| {
        let table = client
            .select(
                "SELECT path, file_hash FROM pgokf.concepts WHERE bundle_id = $1",
                None,
                &[bundle_id.into()],
            )
            .map_err(|error| spi_error("failed to load stored concept hashes", &error))?;
        let mut stored = BTreeMap::new();
        for row in table {
            let path = row
                .get::<String>(1)
                .map_err(|error| spi_error("failed to read stored concept path", &error))?
                .ok_or_else(|| {
                    CatalogError::internal("stored concept path is NULL", Path::new(""))
                })?;
            let file_hash = row
                .get::<String>(2)
                .map_err(|error| spi_error("failed to read stored concept hash", &error))?
                .ok_or_else(|| {
                    CatalogError::internal("stored concept hash is NULL", Path::new(""))
                })?;
            stored.insert(path, file_hash);
        }
        Ok(stored)
    })
}

fn sync_config_from_gucs(root: &Path) -> SyncConfig {
    SyncConfig::new(root)
        .with_max_file_bytes(u64::try_from(guc::max_file_bytes()).unwrap_or(u64::MAX))
        .with_max_files(guc::max_bundle_files())
}

fn parser_limits_from_gucs() -> ParserLimits {
    ParserLimits {
        max_file_bytes: guc::max_file_bytes(),
        max_frontmatter_bytes: guc::max_frontmatter_bytes(),
    }
}

/// Read and parse every added/updated file into the seam payload.
///
/// Strict policy: the first malformed file aborts the sync with SQLSTATE
/// `22023` and the offending bundle-relative path, so a partial projection is
/// never committed (the surrounding transaction rolls back).
///
/// The staged concepts are fully materialized rather than streamed: the
/// ordered projection seam ([`crate::catalog::links::project`] /
/// [`crate::catalog::provenance::project`]) consumes the complete slice after
/// the concept rows are written, and the links pass resolves internal edges
/// against the whole staged set. Peak memory is bounded by the same limits that
/// bound the scan — `pgokf.max_bundle_files` caps the number of parsed
/// concepts and `pgokf.max_file_bytes` caps each source file — while the SQL
/// writes themselves are chunked ([`upsert_concepts`],
/// [`replace_concept_metadata`]) so no single statement is unbounded.
fn stage_changed_concepts(
    root: &Path,
    delta: &BundleDelta,
) -> Result<Vec<StagedConcept>, CatalogError> {
    let limits = parser_limits_from_gucs();
    delta
        .to_parse
        .iter()
        .map(|metadata| {
            let absolute = root.join(&metadata.path);
            let bytes = std::fs::read(&absolute).map_err(|error| {
                CatalogError::internal(
                    format!("failed to read bundle file: {error}"),
                    &metadata.path,
                )
            })?;
            let concept = parse_concept(&bytes, &metadata.path, limits).map_err(|error| {
                CatalogError::invalid_parameter(
                    format!("failed to parse OKF concept: {error}"),
                    &metadata.path,
                )
            })?;
            let modified_at_epoch = metadata
                .modified_at
                .and_then(|instant| instant.duration_since(UNIX_EPOCH).ok())
                .map(|duration| duration.as_secs_f64());
            Ok(StagedConcept {
                concept,
                file_hash: metadata.hash.clone(),
                modified_at_epoch,
            })
        })
        .collect()
}

fn delete_removed_concepts(bundle_id: i64, removed_paths: &[String]) -> Result<(), CatalogError> {
    if removed_paths.is_empty() {
        return Ok(());
    }
    Spi::run_with_args(
        "DELETE FROM pgokf.concepts WHERE bundle_id = $1 AND path = ANY($2)",
        &[bundle_id.into(), removed_paths.to_vec().into()],
    )
    .map_err(|error| spi_error("failed to delete removed concepts", &error))
}

/// Bulk-upsert every staged concept in bounded batches, recomputing each
/// weighted `body_tsv`.
///
/// Concepts are processed in chunks of [`BATCH_SIZE`] so a very large bundle
/// never builds one unbounded statement or parameter array. Each chunk runs a
/// single array-unnest `INSERT ... SELECT` that reproduces, row for row, the
/// same columns and `tsvector` the row-by-row upsert produced: title (weight
/// `A`), the space-joined tags/type/description (weight `B`), and body text
/// (weight `D`). The `ON CONFLICT` branch refreshes `indexed_at`; unchanged
/// concepts never reach this statement, which preserves their `indexed_at`.
///
/// The batch relies on each `(bundle_id, id)` conflict key being unique within
/// a chunk — guaranteed because concept IDs are derived from the bundle-unique
/// source path (`concepts_bundle_path_key`), so `ON CONFLICT DO UPDATE` never
/// sees a row twice in one statement.
fn upsert_concepts(bundle_id: i64, staged: &[StagedConcept]) -> Result<(), CatalogError> {
    // Tags are the one ragged (per-row `text[]`) column, so they cannot be bound
    // as a single rectangular array. Instead the chunk's tags are flattened into
    // `$7` and each row carries an inclusive 1-based slice window
    // (`$12` lo, `$13` hi) into it; `($7)[lo:hi]` rebuilds the row's array (empty
    // when hi < lo). The inner subquery materializes that slice once so the
    // stored column and the weighted `body_tsv` share it.
    const UPSERT: &str = "
        INSERT INTO pgokf.concepts
            (bundle_id, id, path, type, title, description, tags, resource,
             body_text, file_hash, modified_at, body_tsv)
        SELECT
            $1, t.id, t.path, t.type, t.title, t.description, t.tags, t.resource,
            t.body_text, t.file_hash, pg_catalog.to_timestamp(t.modified_at),
            pg_catalog.setweight(
                pg_catalog.to_tsvector('pg_catalog.english', t.title), 'A')
            || pg_catalog.setweight(
                   pg_catalog.to_tsvector(
                       'pg_catalog.english',
                       pg_catalog.concat_ws(' ',
                           pg_catalog.array_to_string(t.tags, ' '),
                           t.type, t.description)),
                   'B')
            || pg_catalog.setweight(
                   pg_catalog.to_tsvector('pg_catalog.english', t.body_text), 'D')
        FROM (
            SELECT
                d.id, d.path, d.type, d.title, d.description, d.resource,
                d.body_text, d.file_hash, d.modified_at,
                COALESCE(($7::text[])[d.lo:d.hi], ARRAY[]::text[]) AS tags
            FROM unnest(
                     $2::text[], $3::text[], $4::text[], $5::text[], $6::text[],
                     $8::text[], $9::text[], $10::text[], $11::float8[],
                     $12::integer[], $13::integer[])
                 AS d(id, path, type, title, description, resource,
                       body_text, file_hash, modified_at, lo, hi)
        ) AS t
        ON CONFLICT (bundle_id, id) DO UPDATE SET
            path = excluded.path,
            type = excluded.type,
            title = excluded.title,
            description = excluded.description,
            tags = excluded.tags,
            resource = excluded.resource,
            body_text = excluded.body_text,
            file_hash = excluded.file_hash,
            modified_at = excluded.modified_at,
            body_tsv = excluded.body_tsv,
            indexed_at = pg_catalog.now()";

    for chunk in staged.chunks(BATCH_SIZE) {
        let columns = batch::marshal_concepts(chunk);
        Spi::run_with_args(
            UPSERT,
            &[
                bundle_id.into(),
                columns.ids.into(),
                columns.paths.into(),
                columns.types.into(),
                columns.titles.into(),
                columns.descriptions.into(),
                columns.tags_flat.into(),
                columns.resources.into(),
                columns.body_texts.into(),
                columns.file_hashes.into(),
                columns.modified_ats.into(),
                columns.tag_los.into(),
                columns.tag_his.into(),
            ],
        )
        .map_err(|error| spi_error("failed to upsert concepts", &error))?;
    }
    Ok(())
}

/// Replace the producer-metadata rows for every touched concept in bounded
/// batches.
///
/// First every touched concept's stored metadata is cleared (in [`BATCH_SIZE`]
/// chunks of concept IDs, so a concept that now has no metadata still loses its
/// stale rows), then the flattened `(concept_id, key, value)` triples are
/// re-inserted with array-unnest bulk `INSERT`s, also chunked. Each value binds
/// as its compact JSON text and is cast back to `jsonb` in SQL — the same
/// serialization `pgrx::JsonB` performs — so the stored value is byte-identical
/// to the row-by-row binding.
fn replace_concept_metadata(bundle_id: i64, staged: &[StagedConcept]) -> Result<(), CatalogError> {
    const INSERT: &str = "
        INSERT INTO pgokf.concept_metadata (bundle_id, concept_id, key, value)
        SELECT $1, d.concept_id, d.key, d.value::jsonb
        FROM unnest($2::text[], $3::text[], $4::text[])
             AS d(concept_id, key, value)";

    for chunk in staged.chunks(BATCH_SIZE) {
        let concept_ids: Vec<&str> = chunk
            .iter()
            .map(|entry| entry.concept.id.as_str())
            .collect();
        Spi::run_with_args(
            "DELETE FROM pgokf.concept_metadata WHERE bundle_id = $1 AND concept_id = ANY($2)",
            &[bundle_id.into(), concept_ids.into()],
        )
        .map_err(|error| spi_error("failed to clear concept metadata", &error))?;
    }

    let rows = batch::flatten_metadata(staged);
    for start in (0..rows.concept_ids.len()).step_by(BATCH_SIZE) {
        let end = usize::min(start + BATCH_SIZE, rows.concept_ids.len());
        Spi::run_with_args(
            INSERT,
            &[
                bundle_id.into(),
                rows.concept_ids[start..end].to_vec().into(),
                rows.keys[start..end].to_vec().into(),
                rows.values[start..end].to_vec().into(),
            ],
        )
        .map_err(|error| spi_error("failed to insert concept metadata", &error))?;
    }
    Ok(())
}

fn update_bundle_row(bundle_id: i64, sync_hash: &str) -> Result<(), CatalogError> {
    Spi::run_with_args(
        "UPDATE pgokf.bundles b
         SET file_count = (SELECT count(*)::integer
                           FROM pgokf.concepts c
                           WHERE c.bundle_id = b.id),
             last_synced_at = pg_catalog.now(),
             sync_hash = $2
         WHERE b.id = $1",
        &[bundle_id.into(), sync_hash.into()],
    )
    .map_err(|error| spi_error("failed to update bundle sync state", &error))
}

/// The shared sync engine used by both `register_bundle` and
/// `refresh_bundle`; see the module docs for the full step list.
///
/// Callers must already hold the bundle advisory lock and have authorized
/// the operation.
fn run_bundle_sync(bundle_id: i64, canonical_root: &Path) -> Result<SyncReport, CatalogError> {
    let stored = load_stored_hashes(bundle_id)?;
    let config = sync_config_from_gucs(canonical_root);
    let current = discover(&config).map_err(|error| {
        CatalogError::invalid_parameter(format!("bundle scan failed: {error}"), Path::new(""))
    })?;
    let delta = classify_changes(&stored, &current);
    let staged = stage_changed_concepts(canonical_root, &delta)?;

    delete_removed_concepts(bundle_id, &delta.removed_paths)?;
    upsert_concepts(bundle_id, &staged)?;
    replace_concept_metadata(bundle_id, &staged)?;

    // Ordered projection seam: feature modules observe the staged concepts
    // here. Each is a documented no-op until its wave lands; removals need no
    // seam because feature tables cascade from pgokf.concepts.
    crate::catalog::links::project(bundle_id, &staged)?;
    crate::catalog::provenance::project(bundle_id, &staged)?;

    update_bundle_row(bundle_id, &bundle_sync_hash(&current))?;
    Ok(delta.report)
}

fn lookup_bundle_id_by_path(canonical_path: &str) -> Result<Option<i64>, CatalogError> {
    Spi::connect(|client| {
        let table = client
            .select(
                "SELECT id FROM pgokf.bundles WHERE path = $1",
                Some(1),
                &[canonical_path.into()],
            )
            .map_err(|error| spi_error("failed to look up bundle by path", &error))?;
        if table.is_empty() {
            return Ok(None);
        }
        table
            .first()
            .get_one::<i64>()
            .map_err(|error| spi_error("failed to read bundle id", &error))
    })
}

fn insert_bundle_row(
    canonical_path: &str,
    name: Option<String>,
    options: Option<pgrx::JsonB>,
) -> Result<i64, CatalogError> {
    Spi::get_one_with_args::<i64>(
        "INSERT INTO pgokf.bundles (path, name, options)
         VALUES ($1, $2, COALESCE($3, '{}'::jsonb))
         RETURNING id",
        &[canonical_path.into(), name.into(), options.into()],
    )
    .map_err(|error| spi_error("failed to insert bundle row", &error))?
    .ok_or_else(|| CatalogError::internal("bundle insert returned no id", Path::new("")))
}

fn lookup_bundle_path(bundle_id: i64) -> Result<Option<String>, CatalogError> {
    Spi::connect(|client| {
        let table = client
            .select(
                "SELECT path FROM pgokf.bundles WHERE id = $1",
                Some(1),
                &[bundle_id.into()],
            )
            .map_err(|error| spi_error("failed to look up bundle path", &error))?;
        if table.is_empty() {
            return Ok(None);
        }
        table
            .first()
            .get_one::<String>()
            .map_err(|error| spi_error("failed to read bundle path", &error))
    })
}

fn register_bundle_impl(
    path: &str,
    name: Option<String>,
    options: Option<pgrx::JsonB>,
) -> Result<(i64, String, SyncReport), CatalogError> {
    security::authorize_current_user(security::Operation::Register, Path::new(""))?;
    let canonical_root = resolve_bundle_root(path)?;
    // Enforce configured allowed roots when present; a no-op under the interim
    // policy (no roots configured). See [`crate::catalog::config`].
    crate::catalog::config::enforce_allowed_roots(path)?;
    let canonical_text = canonical_path_text(&canonical_root)?.to_owned();

    acquire_bundle_lock(&canonical_text)?;
    if lookup_bundle_id_by_path(&canonical_text)?.is_some() {
        return Err(CatalogError::duplicate_path(
            format!("bundle path {canonical_text} is already registered; use pgokf.refresh_bundle"),
            Path::new(""),
        ));
    }

    let bundle_id = insert_bundle_row(&canonical_text, name, options)?;
    let report = run_bundle_sync(bundle_id, &canonical_root)?;
    Ok((bundle_id, canonical_text, report))
}

fn refresh_bundle_impl(bundle_id: i64) -> Result<(String, SyncReport), CatalogError> {
    security::authorize_current_user(security::Operation::Refresh, Path::new(""))?;
    let stored_path = lookup_bundle_path(bundle_id)?.ok_or_else(|| {
        CatalogError::invalid_parameter(
            format!("bundle {bundle_id} is not registered"),
            Path::new(""),
        )
    })?;

    // Lock on the stored canonical path so refresh serializes with a
    // concurrent register/refresh of the same bundle, then re-validate the
    // root before touching the filesystem.
    acquire_bundle_lock(&stored_path)?;
    let canonical_root = resolve_bundle_root(&stored_path)?;
    let report = run_bundle_sync(bundle_id, &canonical_root)?;
    Ok((stored_path, report))
}

/// SQL-facing sync entry points, installed into the `pgokf` schema.
#[pgrx::pg_schema]
mod pgokf {
    use pgrx::{default, extension_sql, pg_extern};

    use super::{refresh_bundle_impl, register_bundle_impl};
    use crate::catalog::types;

    /// Register an OKF bundle root and synchronize it into the catalog.
    ///
    /// Requires membership in `pgokf_admin`. The path must be absolute,
    /// traversal-free, and canonicalizable; the canonical path is stored and
    /// must not already be registered (SQLSTATE `23505` otherwise — use
    /// `pgokf.refresh_bundle` to re-synchronize).
    #[pg_extern(requires = ["catalog_tables"])]
    fn register_bundle(
        path: &str,
        name: default!(Option<String>, "NULL"),
        options: default!(Option<pgrx::JsonB>, "'{}'"),
    ) -> pgrx::composite_type!('static, "pgokf.bundle_sync_result") {
        let (bundle_id, canonical_path, report) =
            register_bundle_impl(path, name, options).unwrap_or_else(|error| error.raise());
        types::bundle_sync_result(bundle_id, &canonical_path, report)
            .unwrap_or_else(|error| error.raise())
    }

    /// Re-synchronize a registered bundle from its stored canonical path.
    ///
    /// Requires membership in `pgokf_admin`. Only files whose BLAKE3 content
    /// hash changed are re-parsed; unchanged rows are left untouched
    /// (preserving `indexed_at`), and rows for deleted files are removed.
    #[pg_extern(requires = ["catalog_tables"])]
    fn refresh_bundle(
        bundle_id: i64,
    ) -> pgrx::composite_type!('static, "pgokf.bundle_sync_result") {
        let (canonical_path, report) =
            refresh_bundle_impl(bundle_id).unwrap_or_else(|error| error.raise());
        types::bundle_sync_result(bundle_id, &canonical_path, report)
            .unwrap_or_else(|error| error.raise())
    }

    extension_sql!(
        r"
ALTER FUNCTION pgokf.register_bundle(text, text, jsonb)
    SECURITY DEFINER SET search_path = pg_catalog, pg_temp;
ALTER FUNCTION pgokf.refresh_bundle(bigint)
    SECURITY DEFINER SET search_path = pg_catalog, pg_temp;
REVOKE ALL ON FUNCTION pgokf.register_bundle(text, text, jsonb) FROM PUBLIC;
REVOKE ALL ON FUNCTION pgokf.refresh_bundle(bigint) FROM PUBLIC;
GRANT EXECUTE ON FUNCTION pgokf.register_bundle(text, text, jsonb) TO pgokf_admin;
GRANT EXECUTE ON FUNCTION pgokf.refresh_bundle(bigint) TO pgokf_admin;
COMMENT ON FUNCTION pgokf.register_bundle(text, text, jsonb) IS
    'Register an OKF bundle root and synchronize it into the catalog. Admin-only; raises 23505 if the canonical path is already registered.';
COMMENT ON FUNCTION pgokf.refresh_bundle(bigint) IS
    'Incrementally re-synchronize a registered bundle: re-parses only content-changed files, removes rows for deleted files. Admin-only.';
",
        name = "sync_function_hardening",
        requires = [register_bundle, refresh_bundle]
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    struct TempBundle {
        root: PathBuf,
    }

    impl TempBundle {
        fn new() -> Self {
            let nonce = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("clock before epoch")
                .as_nanos();
            let root =
                std::env::temp_dir().join(format!("pgokf-sync-{}-{nonce}", std::process::id()));
            fs::create_dir_all(&root).expect("create temp bundle root");
            Self { root }
        }

        fn write(&self, relative: &str, contents: &str) {
            let path = self.root.join(relative);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).expect("create parent directories");
            }
            fs::write(path, contents).expect("write bundle file");
        }

        fn snapshot(&self) -> Snapshot {
            discover(&SyncConfig::new(&self.root)).expect("discover temp bundle")
        }
    }

    impl Drop for TempBundle {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    #[test]
    fn advisory_lock_key_is_stable_for_the_same_path() {
        // Arrange
        let path = "/srv/bundles/handbook";

        // Act
        let first = advisory_lock_key(path);
        let second = advisory_lock_key(path);

        // Assert
        assert_eq!(first, second);
    }

    #[test]
    fn advisory_lock_key_differs_between_paths() {
        // Arrange & Act
        let handbook = advisory_lock_key("/srv/bundles/handbook");
        let runbooks = advisory_lock_key("/srv/bundles/runbooks");

        // Assert
        assert_ne!(handbook, runbooks);
    }

    #[test]
    fn classify_changes_with_no_stored_state_marks_everything_added() {
        // Arrange
        let bundle = TempBundle::new();
        bundle.write("alpha.md", "alpha");
        bundle.write("nested/beta.md", "beta");
        let current = bundle.snapshot();

        // Act
        let delta = classify_changes(&BTreeMap::new(), &current);

        // Assert
        assert_eq!(delta.report.added, 2);
        assert_eq!(delta.report.updated, 0);
        assert_eq!(delta.report.removed, 0);
        assert_eq!(delta.report.unchanged, 0);
        assert_eq!(delta.to_parse.len(), 2);
        assert!(delta.removed_paths.is_empty());
    }

    #[test]
    fn classify_changes_classifies_each_bucket() {
        // Arrange
        let bundle = TempBundle::new();
        bundle.write("unchanged.md", "same");
        bundle.write("updated.md", "after");
        bundle.write("added.md", "new");
        let current = bundle.snapshot();
        let stored = BTreeMap::from([
            ("unchanged.md".to_owned(), hash_bytes(b"same")),
            ("updated.md".to_owned(), hash_bytes(b"before")),
            ("removed.md".to_owned(), hash_bytes(b"gone")),
        ]);

        // Act
        let delta = classify_changes(&stored, &current);

        // Assert
        assert_eq!(delta.report.added, 1);
        assert_eq!(delta.report.updated, 1);
        assert_eq!(delta.report.removed, 1);
        assert_eq!(delta.report.unchanged, 1);
        assert_eq!(delta.removed_paths, vec!["removed.md".to_owned()]);
        let staged_paths: Vec<_> = delta
            .to_parse
            .iter()
            .map(|metadata| metadata.path.to_string_lossy().into_owned())
            .collect();
        assert_eq!(staged_paths, vec!["added.md", "updated.md"]);
    }

    #[test]
    fn classify_changes_skips_reserved_okf_files() {
        // Arrange
        let bundle = TempBundle::new();
        bundle.write("index.md", "reserved bundle index");
        bundle.write("log.md", "reserved bundle log");
        bundle.write("concept.md", "a real concept");
        let current = bundle.snapshot();

        // Act
        let delta = classify_changes(&BTreeMap::new(), &current);

        // Assert
        assert_eq!(delta.report.added, 1);
        assert_eq!(delta.to_parse.len(), 1);
        assert_eq!(delta.to_parse[0].path.to_string_lossy(), "concept.md");
    }

    #[test]
    fn bundle_sync_hash_is_deterministic_and_content_sensitive() {
        // Arrange
        let bundle = TempBundle::new();
        bundle.write("concept.md", "first");
        let before = bundle.snapshot();

        // Act
        let first = bundle_sync_hash(&before);
        let repeat = bundle_sync_hash(&bundle.snapshot());
        bundle.write("concept.md", "second");
        let after = bundle_sync_hash(&bundle.snapshot());

        // Assert
        assert_eq!(first, repeat);
        assert_ne!(first, after);
    }

    #[test]
    fn bundle_sync_hash_ignores_reserved_files() {
        // Arrange
        let bundle = TempBundle::new();
        bundle.write("concept.md", "content");
        let without_reserved = bundle_sync_hash(&bundle.snapshot());
        bundle.write("index.md", "reserved");

        // Act
        let with_reserved = bundle_sync_hash(&bundle.snapshot());

        // Assert
        assert_eq!(without_reserved, with_reserved);
    }
}
