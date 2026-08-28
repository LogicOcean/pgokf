//! Opt-in verbatim source-byte storage and retrieval (`pgokf.concept_source`).
//!
//! # Two deployment tiers, one toggle
//!
//! This feature makes the durable `store_source` configuration key express two
//! deployment shapes without changing any other behavior:
//!
//! - **Small self-contained tier** (`store_source = true`): the sync engine
//!   retains the source bytes it already read to parse each concept and this
//!   seam persists them into `pgokf.concept_source`, so the *original* files
//!   live inside Postgres. Such an install needs no external object store — the
//!   catalog is the source of truth and the bundle can be reconstructed on disk
//!   byte-for-byte with [`export_sources`].
//! - **Enterprise data-lake tier** (`store_source = false`, the default): the
//!   verbatim files stay in a mounted object store / data lake and Postgres
//!   holds only the metadata-and-search projection. No `concept_source` row is
//!   written, so the default install is byte-for-byte identical to a build
//!   without this module.
//!
//! # Seam contract
//!
//! Everything lives in **this file only** — the sync engine already calls
//! [`project`] inside the advisory-locked atomic transaction (after the concept
//! rows are written), and must not be edited. Removals need no per-source seam:
//! the `(bundle_id, concept_id)` foreign key cascades from `pgokf.concepts`
//! exactly as `concept_metadata`, `links`, and `concept_provenance` do, so
//! deleting a concept (or unregistering a bundle) drops its stored source
//! automatically.
//!
//! # Retrieval surface
//!
//! - [`get_concept_source`](pgokf::get_concept_source) returns the exact stored
//!   bytes to the client (reader-level; no filesystem write, so no path-security
//!   surface). It is `SECURITY DEFINER` and tenant-scoped so each successful read
//!   appends one `get_concept_source` row to the exfiltration access log
//!   ([`crate::catalog::access`]).
//! - [`export_sources`](pgokf::export_sources) reconstructs the bundle on disk
//!   (admin-only), reusing the same destination-directory validation and
//!   `O_NOFOLLOW` file creation as [`crate::catalog::export`] and verifying every
//!   written file against the concept's recorded BLAKE3 `file_hash`.
//!
//! # TOAST compression
//!
//! `raw_content` is a `bytea` and benefits from column compression. The table
//! DDL attempts `lz4` for `raw_content` in an exception-guarded `DO` block:
//! `lz4` is faster than the default `pglz` but is not compiled into every
//! `PostgreSQL` build, so a build without it silently keeps `pglz` rather than
//! failing `CREATE EXTENSION`. Operators on an `lz4`-enabled build get `lz4`
//! automatically and may switch a column's compression at any time with
//! `ALTER TABLE ... ALTER COLUMN ... SET COMPRESSION`.

use std::io::Write;
use std::path::{Component, Path};

use okf_sync::hash_bytes;
use pgrx::datum::DatumWithOid;
use pgrx::heap_tuple::PgHeapTuple;
use pgrx::{AllocatedByRust, Spi, extension_sql};

use crate::catalog::batch::{self, BATCH_SIZE};
use crate::catalog::export;
use crate::catalog::spi_read;
use crate::catalog::types::StagedConcept;
use crate::errors::CatalogError;
use crate::security;

/// Qualified SQL name of the export-result composite type reused by
/// [`export_sources`](pgokf::export_sources).
const EXPORT_RESULT_TYPE: &str = "pgokf.export_result";

/// Rows read per keyset batch when reconstructing a bundle on disk.
///
/// A modest constant keeps peak memory bounded to one batch of `bytea`
/// payloads regardless of bundle size; each `raw_content` is itself already
/// capped at `pgokf.max_file_bytes` by the sync-time scan.
const SOURCE_EXPORT_BATCH_ROWS: i64 = 256;

extension_sql!(
    r"
CREATE TABLE pgokf.concept_source (
    bundle_id   bigint  NOT NULL,
    concept_id  text    NOT NULL,
    raw_content bytea   NOT NULL,
    byte_size   integer NOT NULL,
    tenant_id   text    NOT NULL DEFAULT 'default',
    CONSTRAINT concept_source_pkey PRIMARY KEY (bundle_id, concept_id),
    CONSTRAINT concept_source_concept_fk
        FOREIGN KEY (bundle_id, concept_id)
        REFERENCES pgokf.concepts (bundle_id, id)
        ON DELETE CASCADE
);

-- Multi-tenant isolation (see pgokf.bundles): opt-in-by-usage RLS on the
-- denormalized tenant_id. Not forced, so the SECURITY DEFINER sync path bypasses
-- it to persist a single-tenant bundle's source bytes.
ALTER TABLE pgokf.concept_source ENABLE ROW LEVEL SECURITY;
CREATE POLICY concept_source_tenant_isolation ON pgokf.concept_source
    USING (pg_catalog.current_setting('pgokf.tenant', true) IS NULL
        OR pg_catalog.current_setting('pgokf.tenant', true) = ''
        OR tenant_id = pg_catalog.current_setting('pgokf.tenant', true))
    WITH CHECK (pg_catalog.current_setting('pgokf.tenant', true) IS NULL
        OR pg_catalog.current_setting('pgokf.tenant', true) = ''
        OR tenant_id = pg_catalog.current_setting('pgokf.tenant', true));

-- Prefer lz4 compression for the source bytes when this PostgreSQL build ships
-- it; fall back to the default pglz otherwise. Wrapped in an exception-guarded
-- DO block (a subtransaction) so a build without lz4 keeps pglz instead of
-- aborting CREATE EXTENSION. Operators can change this later with
-- ALTER TABLE pgokf.concept_source ALTER COLUMN raw_content SET COMPRESSION ...
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
COMMENT ON COLUMN pgokf.concept_source.tenant_id IS
    'Multi-tenant owner, denormalized from the concept''s bundle for a local row-level-security predicate; always equals the bundle''s tenant_id.';

GRANT SELECT ON pgokf.concept_source TO pgokf_reader;
",
    name = "source_table",
    requires = ["catalog_tables"]
);

fn spi_error(context: &str, error: &pgrx::spi::Error) -> CatalogError {
    CatalogError::internal(format!("{context}: {error}"), Path::new(""))
}

fn composite_error(error: impl std::fmt::Display) -> CatalogError {
    CatalogError::internal(
        format!("failed to build {EXPORT_RESULT_TYPE} composite: {error}"),
        Path::new(""),
    )
}

/// Persist the verbatim source bytes of every staged concept that carries them.
///
/// Invoked inside the sync transaction after the concept rows are written.
/// Only concepts staged with [`StagedConcept::raw_content`] `= Some(..)` (the
/// `store_source` tier is on) contribute a row; under the default policy every
/// staged concept carries `None`, so this writes nothing and current behavior
/// is unchanged. Each contributing concept is upserted with an array-unnest
/// `INSERT ... ON CONFLICT (bundle_id, concept_id) DO UPDATE` in bounded
/// [`BATCH_SIZE`] chunks, so re-syncing a changed file replaces its stored
/// bytes and a very large bundle never builds one unbounded statement. The
/// `bytea[]` payload binds the source bytes exactly, byte for byte.
///
/// # Errors
///
/// Returns a [`CatalogError`] on any SPI failure, aborting the surrounding sync
/// transaction so a partial projection is never committed.
pub fn project(bundle_id: i64, staged: &[StagedConcept]) -> Result<(), CatalogError> {
    // tenant_id is derived from the bundle (single-tenant) and left untouched on
    // conflict, so re-syncing a concept never rewrites its tenant.
    const UPSERT: &str = "
        INSERT INTO pgokf.concept_source (bundle_id, tenant_id, concept_id, raw_content, byte_size)
        SELECT $1,
               (SELECT b.tenant_id FROM pgokf.bundles b WHERE b.id = $1),
               d.concept_id, d.raw_content, d.byte_size
        FROM unnest($2::text[], $3::bytea[], $4::integer[])
             AS d(concept_id, raw_content, byte_size)
        ON CONFLICT (bundle_id, concept_id) DO UPDATE SET
            raw_content = excluded.raw_content,
            byte_size = excluded.byte_size";

    let columns = batch::marshal_sources(staged);
    let total = columns.concept_ids.len();
    for start in (0..total).step_by(BATCH_SIZE) {
        let end = usize::min(start + BATCH_SIZE, total);
        Spi::run_with_args(
            UPSERT,
            &[
                bundle_id.into(),
                columns.concept_ids[start..end].to_vec().into(),
                columns.contents[start..end].to_vec().into(),
                columns.sizes[start..end].to_vec().into(),
            ],
        )
        .map_err(|error| spi_error("failed to upsert concept source", &error))?;
    }
    Ok(())
}

/// Whether `(bundle_id, concept_id)` names a concept currently in the catalog,
/// visible to the session's tenant.
///
/// `get_concept_source` is `SECURITY DEFINER` (so it may append to the
/// administrator-only access log), which bypasses row-level security; the tenant
/// predicate is therefore inlined here so a scoped session cannot probe another
/// tenant's concepts through the "no source stored" vs "no such concept"
/// distinction.
fn concept_exists(bundle_id: i64, concept_id: &str) -> Result<bool, CatalogError> {
    Spi::get_one_with_args::<bool>(
        "SELECT EXISTS (SELECT 1 FROM pgokf.concepts
         WHERE bundle_id = $1 AND id = $2
           AND (pg_catalog.current_setting('pgokf.tenant', true) IS NULL
             OR pg_catalog.current_setting('pgokf.tenant', true) = ''
             OR tenant_id = pg_catalog.current_setting('pgokf.tenant', true)))",
        &[bundle_id.into(), concept_id.into()],
    )
    .map_err(|error| spi_error("failed to look up concept", &error))
    .map(|exists| exists.unwrap_or(false))
}

/// Read the stored source bytes for one concept, distinguishing an absent
/// concept from a concept whose source was never stored.
///
/// `get_concept_source` runs `SECURITY DEFINER` — it must append to the
/// administrator-only `pgokf_private.access_log` — so it bypasses row-level
/// security and applies the same opt-in tenant filter explicitly to the source
/// read and the concept-existence probe. Every successful read appends one
/// `get_concept_source` exfiltration-audit row.
fn get_concept_source_impl(bundle_id: i64, concept_id: &str) -> Result<Vec<u8>, CatalogError> {
    security::authorize_current_user(security::Operation::Search, Path::new(""))?;

    // A single-row read that tolerates zero rows: `Spi::get_one` raises on an
    // empty result rather than returning `None`, so the presence check goes
    // through `is_empty` (the same pattern the bundle lookups use). `None` here
    // means no `concept_source` row exists for the pair (in this tenant).
    let stored: Option<Vec<u8>> = Spi::connect(|client| {
        let table = client.select(
            "SELECT raw_content FROM pgokf.concept_source
             WHERE bundle_id = $1 AND concept_id = $2
               AND (pg_catalog.current_setting('pgokf.tenant', true) IS NULL
                 OR pg_catalog.current_setting('pgokf.tenant', true) = ''
                 OR tenant_id = pg_catalog.current_setting('pgokf.tenant', true))",
            Some(1),
            &[bundle_id.into(), concept_id.into()],
        )?;
        if table.is_empty() {
            Ok(None)
        } else {
            table.first().get_one::<Vec<u8>>()
        }
    })
    .map_err(|error| spi_error("failed to read concept source", &error))?;

    match stored {
        Some(bytes) => {
            // Exfiltration audit: record the single-concept source read.
            crate::catalog::access::record(
                "get_concept_source",
                bundle_id,
                Some(concept_id),
                None,
            )?;
            Ok(bytes)
        }
        None if concept_exists(bundle_id, concept_id)? => Err(CatalogError::invalid_parameter(
            format!(
                "no source is stored for concept {concept_id} in bundle {bundle_id}; \
                 the bundle was synced with store_source disabled"
            ),
            Path::new(""),
        )),
        None => Err(CatalogError::invalid_parameter(
            format!("no such concept {concept_id} in bundle {bundle_id}"),
            Path::new(""),
        )),
    }
}

/// Reject any stored source path that is not a plain bundle-relative path.
///
/// `pgokf.concepts.path` is written from the sync-time discovery scan, which
/// already refuses absolute paths and `..` traversal, but the reconstruction
/// join re-validates defensively so a corrupted `path` value can never redirect
/// a write outside the destination directory.
fn ensure_bundle_relative(path: &str) -> Result<(), CatalogError> {
    let candidate = Path::new(path);
    let unsafe_path = candidate.components().any(|component| {
        matches!(
            component,
            Component::ParentDir | Component::RootDir | Component::Prefix(_)
        )
    });
    if path.is_empty() || unsafe_path {
        return Err(CatalogError::invalid_parameter(
            format!("stored concept path is not a safe bundle-relative path: {path}"),
            Path::new(""),
        ));
    }
    Ok(())
}

/// One row of the reconstruction read: the concept's id (the keyset cursor),
/// its bundle-relative path, its stored source bytes, and the recorded BLAKE3
/// digest to verify against.
struct SourceRow {
    concept_id: String,
    path: String,
    content: Vec<u8>,
    file_hash: String,
}

/// Read one keyset-paginated batch of stored sources joined to their concept
/// path, ordered by `concept_id`, in its own SPI session so the previous
/// batch's tuples are freed before the next is read.
fn read_source_batch(bundle_id: i64, after: Option<&str>) -> Result<Vec<SourceRow>, CatalogError> {
    let query = if after.is_some() {
        "SELECT s.concept_id, c.path, s.raw_content, c.file_hash
         FROM pgokf.concept_source s
         JOIN pgokf.concepts c ON c.bundle_id = s.bundle_id AND c.id = s.concept_id
         WHERE s.bundle_id = $1 AND s.concept_id > $2
         ORDER BY s.concept_id
         LIMIT $3"
    } else {
        "SELECT s.concept_id, c.path, s.raw_content, c.file_hash
         FROM pgokf.concept_source s
         JOIN pgokf.concepts c ON c.bundle_id = s.bundle_id AND c.id = s.concept_id
         WHERE s.bundle_id = $1
         ORDER BY s.concept_id
         LIMIT $2"
    };

    Spi::connect(|client| {
        let mut args: Vec<DatumWithOid> = Vec::with_capacity(3);
        args.push(bundle_id.into());
        if let Some(cursor) = after {
            args.push(cursor.to_owned().into());
        }
        args.push(SOURCE_EXPORT_BATCH_ROWS.into());

        let table = client
            .select(query, Some(SOURCE_EXPORT_BATCH_ROWS), &args)
            .map_err(|error| spi_error("failed to read concept source batch", &error))?;

        let mut rows = Vec::new();
        for row in table {
            rows.push(SourceRow {
                concept_id: spi_read::required_column(
                    &row,
                    1,
                    "failed to read concept id",
                    "concept id is NULL",
                )?,
                path: spi_read::required_column(
                    &row,
                    2,
                    "failed to read concept path",
                    "concept path is NULL",
                )?,
                content: spi_read::required_column::<Vec<u8>>(
                    &row,
                    3,
                    "failed to read stored source",
                    "stored source is NULL",
                )?,
                file_hash: spi_read::required_column(
                    &row,
                    4,
                    "failed to read concept file hash",
                    "concept file hash is NULL",
                )?,
            });
        }
        Ok(rows)
    })
}

/// Reconstruct one stored source file under `dir`, verifying it against the
/// recorded BLAKE3 digest, and return its byte length.
///
/// The path is re-validated as bundle-relative and the file is created with the
/// same `O_NOFOLLOW` open [`crate::catalog::export`] uses, so a symlink planted
/// at the target refuses the write rather than redirecting it. Verification
/// runs on the exact buffer that is written (`write_all` writes it byte for
/// byte), so a mismatch means the stored bytes disagree with the concept's
/// recorded hash — a corruption the caller must not silently reconstruct.
fn reconstruct_file(dir: &Path, row: &SourceRow) -> Result<u64, CatalogError> {
    ensure_bundle_relative(&row.path)?;

    let actual = hash_bytes(&row.content);
    if actual != row.file_hash {
        return Err(CatalogError::internal(
            format!(
                "stored source for {} does not match its recorded file hash \
                 (expected {}, computed {actual})",
                row.path, row.file_hash
            ),
            Path::new(&row.path),
        ));
    }

    let target = dir.join(&row.path);
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent).map_err(|error| {
            CatalogError::internal(
                format!(
                    "failed to create export directory {}: {error}",
                    parent.display()
                ),
                Path::new(&row.path),
            )
        })?;
        // `create_dir_all` follows a pre-existing directory symlink on any
        // intermediate component, and the `O_NOFOLLOW` open below only guards
        // the final path component — so a planted symlink at e.g. `dir/nested`
        // could otherwise redirect the write outside the validated destination.
        // Re-resolve the materialized parent with symlink-escape-safe
        // containment against `dir` and refuse if it escapes.
        let root = [dir.to_path_buf()];
        crate::security::canonicalize_contained_path(parent, &root, Path::new(&row.path))?;
    }

    let mut file = export::create_export_file(&target)?;
    file.write_all(&row.content).map_err(|error| {
        CatalogError::internal(
            format!("failed to write export file {}: {error}", target.display()),
            Path::new(&row.path),
        )
    })?;

    Ok(u64::try_from(row.content.len()).unwrap_or(u64::MAX))
}

/// Confirm a bundle is registered before any file is written.
fn ensure_bundle_exists(bundle_id: i64) -> Result<(), CatalogError> {
    let exists = Spi::get_one_with_args::<bool>(
        "SELECT EXISTS (SELECT 1 FROM pgokf.bundles WHERE id = $1)",
        &[bundle_id.into()],
    )
    .map_err(|error| spi_error("failed to look up bundle", &error))?
    .unwrap_or(false);
    if exists {
        Ok(())
    } else {
        Err(CatalogError::invalid_parameter(
            format!("bundle {bundle_id} is not registered"),
            Path::new(""),
        ))
    }
}

/// Clamp a row count into the `bigint` range of the result composite.
fn count_to_i64(count: usize) -> i64 {
    i64::try_from(count).unwrap_or(i64::MAX)
}

/// The reconstruction summary returned by one [`export_sources`] call.
struct SourceExportSummary {
    bundle_id: i64,
    dest_dir: String,
    files_written: i64,
    bytes_written: i64,
}

/// Authorize, validate the destination, and reconstruct every stored source
/// file for one bundle on disk.
fn export_sources_impl(
    bundle_id: i64,
    dest_dir: &str,
) -> Result<SourceExportSummary, CatalogError> {
    security::authorize_current_user(security::Operation::Register, Path::new(""))?;
    // Write-side tenant confinement: a scoped session may only reconstruct its
    // own tenant's bundle. Checked before the directory is validated (a filesystem
    // side effect) so a cross-tenant or absent id looks identically unknown.
    security::enforce_bundle_tenant(bundle_id)?;
    ensure_bundle_exists(bundle_id)?;
    let dir = export::validate_dest_dir(dest_dir)?;

    let mut cursor: Option<String> = None;
    let mut files: usize = 0;
    let mut total_bytes: u128 = 0;
    loop {
        let batch = read_source_batch(bundle_id, cursor.as_deref())?;
        if batch.is_empty() {
            break;
        }
        let is_last = i64::try_from(batch.len()).unwrap_or(i64::MAX) < SOURCE_EXPORT_BATCH_ROWS;
        for row in &batch {
            total_bytes += u128::from(reconstruct_file(&dir, row)?);
            files += 1;
        }
        // Advance the keyset on the ordering column (concept_id), so the next
        // batch's `s.concept_id > $2` predicate resumes the total order exactly.
        cursor = batch.last().map(|row| row.concept_id.clone());
        if is_last {
            break;
        }
    }

    // Exfiltration audit: record who reconstructed which bundle and where.
    let dest = dir.to_string_lossy();
    crate::catalog::access::record("export_sources", bundle_id, None, Some(dest.as_ref()))?;

    Ok(SourceExportSummary {
        bundle_id,
        dest_dir: dir.to_string_lossy().into_owned(),
        files_written: count_to_i64(files),
        bytes_written: i64::try_from(total_bytes).unwrap_or(i64::MAX),
    })
}

/// Pack a [`SourceExportSummary`] into a `pgokf.export_result` heap tuple.
///
/// The composite is shared with [`crate::catalog::export`]; for a source
/// reconstruction the reconstructed-file count is reported in `concepts_rows`
/// and the per-table counters that do not apply are zero.
fn source_export_result_tuple(
    summary: SourceExportSummary,
) -> Result<PgHeapTuple<'static, AllocatedByRust>, CatalogError> {
    let mut tuple = PgHeapTuple::new_composite_type(EXPORT_RESULT_TYPE).map_err(composite_error)?;
    tuple
        .set_by_name("bundle_id", summary.bundle_id)
        .map_err(composite_error)?;
    tuple
        .set_by_name("dest_dir", summary.dest_dir)
        .map_err(composite_error)?;
    tuple
        .set_by_name("concepts_rows", summary.files_written)
        .map_err(composite_error)?;
    tuple
        .set_by_name("metadata_rows", 0_i64)
        .map_err(composite_error)?;
    tuple
        .set_by_name("links_rows", 0_i64)
        .map_err(composite_error)?;
    tuple
        .set_by_name("provenance_rows", 0_i64)
        .map_err(composite_error)?;
    tuple
        .set_by_name("bytes_written", summary.bytes_written)
        .map_err(composite_error)?;
    Ok(tuple)
}

/// SQL-facing source retrieval entry points, installed into the `pgokf` schema.
#[pgrx::pg_schema]
mod pgokf {
    use pgrx::{extension_sql, pg_extern};

    use super::{export_sources_impl, get_concept_source_impl, source_export_result_tuple};

    /// Return the verbatim stored source bytes of one concept.
    ///
    /// Reader-level: available to `pgokf_reader` and `pgokf_admin`. Returns the
    /// exact bytes stored when the bundle was synced with `store_source`
    /// enabled — the same disclosure level as the concept's `body_text`, and
    /// delivered to the client with no filesystem write. Raises SQLSTATE
    /// `22023` when the concept exists but no source was stored (the bundle was
    /// synced with `store_source` disabled) or when no such concept exists.
    #[pg_extern(stable, requires = ["source_table"])]
    fn get_concept_source(bundle_id: i64, concept_id: &str) -> Vec<u8> {
        get_concept_source_impl(bundle_id, concept_id).unwrap_or_else(|error| error.raise())
    }

    /// Reconstruct a bundle's stored source files on the server filesystem.
    ///
    /// Writes every concept's verbatim source bytes to
    /// `dest_dir/<concept path>`, recreating the bundle-relative directory tree
    /// and verifying each written file against the concept's recorded BLAKE3
    /// `file_hash`. Returns a `pgokf.export_result` whose `concepts_rows` is the
    /// number of files reconstructed and `bytes_written` their total size (the
    /// other per-table counters are zero). Requires membership in `pgokf_admin`.
    /// The destination is validated exactly like a bundle root: absolute,
    /// traversal-free, canonical, contained within `pgokf.allowed_roots` when
    /// configured, and writable; files are created with `O_NOFOLLOW` so a
    /// planted symlink cannot redirect a write. Raises SQLSTATE `22023` for an
    /// unknown bundle or an invalid/missing directory, `42501` for a directory
    /// the server cannot write, and `XX000` when a stored source fails its
    /// BLAKE3 hash check (a corruption/integrity condition, not caller input);
    /// the hash is verified before any file is created, so a mismatch aborts
    /// the export without writing.
    #[pg_extern(requires = ["export_result_type", "source_table"])]
    fn export_sources(
        bundle_id: i64,
        dest_dir: &str,
    ) -> pgrx::composite_type!('static, "pgokf.export_result") {
        let summary =
            export_sources_impl(bundle_id, dest_dir).unwrap_or_else(|error| error.raise());
        source_export_result_tuple(summary).unwrap_or_else(|error| error.raise())
    }

    extension_sql!(
        r"
ALTER FUNCTION pgokf.get_concept_source(bigint, text)
    SECURITY DEFINER SET search_path = pg_catalog, pg_temp;
ALTER FUNCTION pgokf.export_sources(bigint, text)
    SECURITY DEFINER SET search_path = pg_catalog, pg_temp;
REVOKE ALL ON FUNCTION pgokf.get_concept_source(bigint, text) FROM PUBLIC;
REVOKE ALL ON FUNCTION pgokf.export_sources(bigint, text) FROM PUBLIC;
GRANT EXECUTE ON FUNCTION pgokf.get_concept_source(bigint, text) TO pgokf_reader;
GRANT EXECUTE ON FUNCTION pgokf.export_sources(bigint, text) TO pgokf_admin;
COMMENT ON FUNCTION pgokf.get_concept_source(bigint, text) IS
    'Return the verbatim stored source bytes of one concept as bytea. Reader-level (same disclosure as body_text), SECURITY DEFINER and tenant-scoped so it can append one get_concept_source row to the exfiltration access log on each successful read. Raises 22023 when the concept exists but no source was stored, or when no such concept exists.';
COMMENT ON FUNCTION pgokf.export_sources(bigint, text) IS
    'Reconstruct a bundle''s stored source files under dest_dir, recreating the bundle-relative tree and verifying each file against its BLAKE3 file_hash; returns pgokf.export_result (concepts_rows = files written, bytes_written = total bytes). Admin-only; dest_dir must be an existing, writable, canonical directory contained within pgokf.allowed_roots when configured. Raises 22023 (bad bundle/dir), 42501 (dir not writable), or XX000 (a stored source fails its hash check, verified before any write).';
",
        name = "source_function_hardening",
        requires = [get_concept_source, export_sources]
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ensure_bundle_relative_accepts_nested_paths() {
        // Arrange & Act & Assert: ordinary bundle-relative paths pass.
        assert!(ensure_bundle_relative("alpha.md").is_ok());
        assert!(ensure_bundle_relative("nested/beta.md").is_ok());
    }

    #[test]
    fn ensure_bundle_relative_rejects_absolute_paths() {
        // Arrange & Act
        let error =
            ensure_bundle_relative("/etc/passwd").expect_err("absolute paths must be rejected");

        // Assert
        assert_eq!(error.sqlstate(), "22023");
    }

    #[test]
    fn ensure_bundle_relative_rejects_parent_traversal() {
        // Arrange & Act
        let error =
            ensure_bundle_relative("../escape.md").expect_err("parent traversal must be rejected");

        // Assert
        assert_eq!(error.sqlstate(), "22023");
    }

    #[test]
    fn ensure_bundle_relative_rejects_empty_path() {
        // Arrange & Act
        let error = ensure_bundle_relative("").expect_err("empty paths must be rejected");

        // Assert
        assert_eq!(error.sqlstate(), "22023");
    }
}
