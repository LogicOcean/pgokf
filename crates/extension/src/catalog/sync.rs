//! The shared register/refresh engine.
//!
//! `pgokf.register_bundle`, `pgokf.refresh_bundle`, and
//! `pgokf.register_bundle_content` all funnel into [`run_bundle_sync`], which is
//! a single-transaction, set-based diff generic over the [`ByteSource`] seam so
//! the identical downstream pipeline serves both the on-disk
//! [`FilesystemSource`] and the mountless in-memory [`ContentSource`]:
//!
//! 1. serialize on a bundle-scoped advisory lock ([`advisory_lock_key`]);
//! 2. load the stored `path -> file_hash` projection for the bundle;
//! 3. take the current `{path, blake3_hash}` snapshot from the [`ByteSource`]
//!    (the filesystem source uses [`okf_sync::discover`] — symlink-escape safe,
//!    size/count limited from the `pgokf.*` GUCs; the content source hashes the
//!    caller-supplied bytes in memory);
//! 4. classify changes against the stored hashes ([`classify_changes`]) so
//!    unchanged rows are never rewritten and `indexed_at` is preserved;
//! 5. parse only added/updated files with [`okf_parser::parse_concept`];
//! 6. delete removed rows, upsert changed rows (recomputing the weighted
//!    `body_tsv`), and replace `concept_metadata` for touched concepts;
//! 7. run the ordered projection seam — [`crate::catalog::links::project`],
//!    then a bundle-wide link re-resolution
//!    ([`crate::catalog::links::reresolve_bundle`]) so inbound edges of
//!    *unchanged* concepts reflect targets added or removed during this sync,
//!    then [`crate::catalog::provenance::project`], then
//!    [`crate::catalog::source::project`] (opt-in verbatim source-byte
//!    storage, a no-op under the default `store_source`-off policy) — with the
//!    staged concepts;
//! 8. update the bundle row (file count, `last_synced_at`, aggregate
//!    `sync_hash`) last.
//!
//! Because `pgrx` functions execute inside the caller's transaction and every
//! failure is raised as a `PostgreSQL` error, a failed sync rolls back
//! atomically. Under the default `default_strict` policy the first malformed
//! file aborts the sync, so a partial projection is never committed; when
//! `default_strict` is disabled a malformed file is instead logged as a
//! warning and skipped, and the rest of the bundle registers.
//!
//! # Path containment
//!
//! Bundle roots are validated with [`crate::security::validate_path_syntax`]
//! (absolute, no NUL, no `..`) and canonicalized before use, and
//! [`okf_sync::discover`] rejects symlinks that escape the canonical root.
//! Allowed-roots containment is enforced at registration: [`register_bundle_impl`]
//! calls [`crate::catalog::config::enforce_allowed_roots`], so when an
//! administrator has configured `allowed_roots` a candidate path must resolve
//! inside one of them (symlink-escape-safe containment via
//! [`crate::security::canonicalize_contained_path`]); a path that escapes every
//! configured root is rejected with SQLSTATE `22023`. When no roots are
//! configured the interim policy applies — any absolute, canonical,
//! traversal-free path is accepted — and in both cases registration remains
//! restricted to the ingest tier `pgokf_writer` (which `pgokf_admin`
//! inherits).

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use okf_parser::{
    ParserLimits, index_okf_version, is_reserved_log, is_reserved_path, is_supported_okf_version,
    parent_directory, parse_concept,
};
use okf_sync::{FileMetadata, SyncConfig, SyncReport, discover, hash_bytes};
use pgrx::Spi;

use crate::catalog::batch::{self, BATCH_SIZE};
use crate::catalog::spi_read;
use crate::catalog::types::{StagedConcept, count_to_i32};
use crate::errors::CatalogError;
use crate::guc;
use crate::security;

/// Namespace prefix mixed into every advisory-lock key so `pgokf` locks
/// cannot collide with another subsystem hashing similar strings.
const ADVISORY_LOCK_NAMESPACE: &str = "pgokf.bundle";

/// The catalog-mutating operation a [`run_bundle_sync`] call is serving.
///
/// Threaded into the sync so the audit row it appends and the change
/// notification it emits name the right operation. The wire names match the
/// `pgokf_private.sync_log.op` domain and the `pgokf.list_sync_log` contract.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SyncOp {
    /// A first-time filesystem registration (`pgokf.register_bundle`).
    Register,
    /// A filesystem re-synchronization (`pgokf.refresh_bundle`).
    Refresh,
    /// An in-memory content ingest/resync (`pgokf.register_bundle_content`).
    Content,
}

impl SyncOp {
    /// The wire name recorded in `sync_log.op` and emitted in the notification.
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Register => "register",
            Self::Refresh => "refresh",
            Self::Content => "content",
        }
    }
}

/// Per-input character bounds for the three weighted `to_tsvector` operands
/// (title `A`, metadata `B`, body `D`) whose concatenation forms `body_tsv`.
///
/// `PostgreSQL` caps a `tsvector` at `MAXSTRPOS` (`2^20 - 1` = `1_048_575`
/// bytes) and raises SQLSTATE `54000` ("string is too long for tsvector") when
/// a document exceeds it — reachable for a body with hundreds of thousands of
/// distinct tokens even while it stays within `pgokf.max_file_bytes`. Left
/// unbounded, one such file would abort the whole sync and the bundle could
/// never register.
///
/// The size the limit checks is the lexeme pool **plus** per-occurrence
/// position data; in the worst case (single-character, four-byte lexemes each
/// separated by one byte) that total is bounded by `4 × (input characters)`.
/// Crucially the check applies to the **concatenated** vector, so bounding only
/// the body is insufficient — the unbounded `A`/`B` operands push the combined
/// vector over the limit. Bounding all three so their combined worst case stays
/// under `MAXSTRPOS` makes `54000` structurally impossible:
/// `4 × (TITLE + META + BODY) = 4 × 220_000 = 880_000 < 1_048_575` (≈ 16 %
/// headroom). Normal concepts are far smaller than these bounds, so full-text
/// quality is unaffected; only pathologically large inputs have their
/// *indexing* text truncated — every column is still stored and returned in
/// full.
const TITLE_TSV_CHAR_LIMIT: i32 = 4_000;
const META_TSV_CHAR_LIMIT: i32 = 16_000;
const BODY_TSV_CHAR_LIMIT: i32 = 200_000;

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
pub fn classify_changes(
    stored: &BTreeMap<String, String>,
    current: &[FileMetadata],
) -> BundleDelta {
    let mut delta = BundleDelta::default();
    let mut current_paths = BTreeSet::new();

    for metadata in current {
        let path_text = metadata.path.to_string_lossy().into_owned();
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
pub fn bundle_sync_hash(current: &[FileMetadata]) -> String {
    let mut buffer = String::new();
    for metadata in current {
        let path_text = metadata.path.to_string_lossy();
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
/// resolved filesystem path. Allowed-roots containment is enforced separately
/// by [`crate::catalog::config::enforce_allowed_roots`]; see the module docs.
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

pub(crate) fn acquire_bundle_lock(canonical_path: &str) -> Result<(), CatalogError> {
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
            let path: String = spi_read::required_column(
                &row,
                1,
                "failed to read stored concept path",
                "stored concept path is NULL",
            )?;
            let file_hash: String = spi_read::required_column(
                &row,
                2,
                "failed to read stored concept hash",
                "stored concept hash is NULL",
            )?;
            stored.insert(path, file_hash);
        }
        Ok(stored)
    })
}

/// Build the discovery configuration from the per-session GUC ceilings and the
/// durable `default_exclude` globs.
///
/// The resource ceilings come from the `pgokf.*` GUCs; the exclude patterns are
/// the configured `default_exclude` list (combined with any per-call excludes,
/// of which there are currently none in the SQL surface). Excludes always win
/// over includes in [`okf_sync::discover`].
fn sync_config_from_gucs(root: &Path, exclude: &[String]) -> SyncConfig {
    SyncConfig::new(root)
        .with_max_file_bytes(u64::try_from(guc::max_file_bytes()).unwrap_or(u64::MAX))
        .with_max_files(guc::max_bundle_files())
        .with_exclude(exclude.iter().cloned())
}

fn parser_limits_from_gucs() -> ParserLimits {
    ParserLimits {
        max_file_bytes: guc::max_file_bytes(),
        max_frontmatter_bytes: guc::max_frontmatter_bytes(),
    }
}

/// The seam that decouples the shared classify/parse/upsert/project pipeline
/// from *where the bundle bytes come from*.
///
/// A `ByteSource` yields a whole-bundle snapshot of `{path, blake3_hash}` pairs
/// ([`ByteSource::snapshot`]) that [`classify_changes`] diffs against the stored
/// projection, produces the raw bytes for any changed path on demand
/// ([`ByteSource::read_bytes`]), and reports the bundle-root OKF format version
/// ([`ByteSource::root_okf_version`]). [`run_bundle_sync`] is generic over it, so
/// the identical downstream logic serves both the on-disk
/// [`FilesystemSource`] (walk + BLAKE3 + `std::fs::read`) and the mountless
/// [`ContentSource`] (caller-supplied `(paths, contents)` held in memory).
pub(crate) trait ByteSource {
    /// The full bundle snapshot as `{path, blake3_hash}` entries. Reserved OKF
    /// files (`index.md` / `log.md`) may be present; [`classify_changes`] and
    /// [`bundle_sync_hash`] skip them, exactly as for the filesystem scan.
    ///
    /// `exclude` carries the durable `default_exclude` globs so the filesystem
    /// scan can honor them; sources with no notion of on-disk exclusion ignore
    /// it.
    ///
    /// # Errors
    ///
    /// Returns a [`CatalogError`] when the snapshot cannot be produced (for the
    /// filesystem source, when the directory scan fails or a resource ceiling
    /// is exceeded).
    fn snapshot(&self, exclude: &[String]) -> Result<Vec<FileMetadata>, CatalogError>;

    /// Produce the raw bytes for one changed bundle-relative path.
    ///
    /// # Errors
    ///
    /// Returns a [`CatalogError`] when the bytes cannot be produced (an I/O
    /// failure for the filesystem source, or a path the content source was
    /// never given).
    fn read_bytes(&self, path: &Path) -> Result<Vec<u8>, CatalogError>;

    /// The OKF format version declared in the bundle-root `index.md`, when one
    /// is present and carries a scalar `okf_version`; `None` otherwise. Fully
    /// defensive so it can never abort a sync.
    fn root_okf_version(&self) -> Option<String>;
}

/// On-disk [`ByteSource`]: the historical register/refresh behavior, unchanged.
///
/// The snapshot is [`okf_sync::discover`] (symlink-escape-safe walk + BLAKE3
/// hashing, size/count limited from the `pgokf.*` GUCs); changed bytes are read
/// with `std::fs::read`; the version comes from the bundle-root `index.md`.
pub(crate) struct FilesystemSource {
    canonical_root: PathBuf,
}

impl FilesystemSource {
    /// Wrap an already-resolved, canonical bundle root.
    pub(crate) fn new(canonical_root: PathBuf) -> Self {
        Self { canonical_root }
    }
}

impl ByteSource for FilesystemSource {
    fn snapshot(&self, exclude: &[String]) -> Result<Vec<FileMetadata>, CatalogError> {
        let config = sync_config_from_gucs(&self.canonical_root, exclude);
        let snapshot = discover(&config).map_err(|error| {
            CatalogError::invalid_parameter(format!("bundle scan failed: {error}"), Path::new(""))
        })?;
        Ok(snapshot.files().values().cloned().collect())
    }

    fn read_bytes(&self, path: &Path) -> Result<Vec<u8>, CatalogError> {
        let absolute = self.canonical_root.join(path);
        std::fs::read(&absolute).map_err(|error| {
            CatalogError::internal(format!("failed to read bundle file: {error}"), path)
        })
    }

    fn root_okf_version(&self) -> Option<String> {
        read_root_okf_version(&self.canonical_root)
    }
}

/// In-memory [`ByteSource`]: the mountless content-ingestion path.
///
/// Built from caller-supplied `(paths, contents)` (validated as safe
/// bundle-relative paths by [`crate::catalog::content`] before construction).
/// Each content's BLAKE3 hash is computed once with [`okf_sync::hash_bytes`] so
/// the shared pipeline diffs it against the stored projection exactly as it does
/// the filesystem hashes; the bytes are served straight from memory. The
/// bundle-root version is read from a provided root `index.md`, if any.
pub(crate) struct ContentSource {
    /// Snapshot entries, sorted by path for a deterministic `sync_hash`.
    entries: Vec<FileMetadata>,
    /// The exact bytes for every provided path, keyed by path.
    contents: BTreeMap<PathBuf, Vec<u8>>,
}

impl ContentSource {
    /// Build a content source from validated, equal-length `paths` / `contents`.
    ///
    /// Callers ([`crate::catalog::content`]) must already have rejected NULL and
    /// traversing paths and enforced the `pgokf.max_bundle_files` /
    /// `pgokf.max_file_bytes` ceilings; this constructor only hashes each
    /// content and materializes the snapshot.
    pub(crate) fn new(paths: Vec<String>, contents: Vec<Vec<u8>>) -> Self {
        let mut entries = Vec::with_capacity(paths.len());
        let mut map = BTreeMap::new();
        for (path, content) in paths.into_iter().zip(contents) {
            let hash = hash_bytes(&content);
            let size_bytes = u64::try_from(content.len()).unwrap_or(u64::MAX);
            let path_buf = PathBuf::from(path);
            entries.push(FileMetadata {
                path: path_buf.clone(),
                hash,
                size_bytes,
                modified_at: None,
            });
            map.insert(path_buf, content);
        }
        entries.sort_by(|left, right| left.path.cmp(&right.path));
        Self {
            entries,
            contents: map,
        }
    }
}

impl ByteSource for ContentSource {
    fn snapshot(&self, _exclude: &[String]) -> Result<Vec<FileMetadata>, CatalogError> {
        Ok(self.entries.clone())
    }

    fn read_bytes(&self, path: &Path) -> Result<Vec<u8>, CatalogError> {
        self.contents.get(path).cloned().ok_or_else(|| {
            CatalogError::internal(
                format!("no content was provided for path {}", path.display()),
                path,
            )
        })
    }

    fn root_okf_version(&self) -> Option<String> {
        let index = self.contents.get(Path::new("index.md"))?;
        index_okf_version(index, guc::max_frontmatter_bytes())
    }
}

/// The concepts staged for the projection seam plus the files skipped over.
///
/// `skipped` is always empty under the strict policy (a malformed file aborts
/// the sync instead); under the non-strict policy it collects the
/// bundle-relative paths of files that failed to parse and were logged and
/// passed over, so the caller can reconcile the sync report with what was
/// actually indexed.
struct StagingOutcome {
    staged: Vec<StagedConcept>,
    skipped: Vec<PathBuf>,
}

/// Read and parse every added/updated file into the seam payload.
///
/// The `strict` flag threads the durable `default_strict` policy through the
/// staging loop:
///
/// - `strict == true` (default): the first malformed file aborts the sync with
///   SQLSTATE `22023` and the offending bundle-relative path, so a partial
///   projection is never committed (the surrounding transaction rolls back);
/// - `strict == false`: a malformed file is logged as a warning, recorded in
///   [`StagingOutcome::skipped`], and passed over, so the rest of the bundle
///   still registers. A file that cannot be *read* (an I/O failure, not a parse
///   failure) remains a hard error in both modes.
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
fn stage_changed_concepts<S: ByteSource>(
    source: &S,
    delta: &BundleDelta,
    strict: bool,
    store_source: bool,
) -> Result<StagingOutcome, CatalogError> {
    let limits = parser_limits_from_gucs();
    let mut staged = Vec::with_capacity(delta.to_parse.len());
    let mut skipped = Vec::new();
    for metadata in &delta.to_parse {
        let bytes = source.read_bytes(&metadata.path)?;
        let concept = match parse_concept(&bytes, &metadata.path, limits) {
            Ok(concept) => concept,
            Err(error) if strict => {
                return Err(CatalogError::invalid_parameter(
                    format!("failed to parse OKF concept: {error}"),
                    &metadata.path,
                ));
            }
            Err(error) => {
                pgrx::warning!(
                    "pgokf: skipping malformed OKF concept {} (default_strict is off): {error}",
                    metadata.path.display()
                );
                skipped.push(metadata.path.clone());
                continue;
            }
        };
        let modified_at_epoch = metadata
            .modified_at
            .and_then(|instant| instant.duration_since(UNIX_EPOCH).ok())
            .map(|duration| duration.as_secs_f64());
        // Retain the already-read source buffer only under the small-install
        // `store_source` tier; otherwise drop it so default behavior is
        // byte-for-byte unchanged and no source bytes are held or persisted.
        // The buffer is moved rather than cloned — after `parse_concept`
        // returns the borrow is released, so no extra allocation or I/O occurs.
        let raw_content = if store_source { Some(bytes) } else { None };
        staged.push(StagedConcept {
            concept,
            file_hash: metadata.hash.clone(),
            modified_at_epoch,
            raw_content,
        });
    }
    Ok(StagingOutcome { staged, skipped })
}

/// Reconcile the sync report with the files skipped under the non-strict
/// policy.
///
/// Each skipped path was classified as `added` or `updated` (it was staged for
/// parsing) but was never written, so the corresponding bucket is decremented
/// to keep the returned report an honest account of what was actually indexed.
/// A path present in the stored projection was an `updated`; one absent was an
/// `added`.
fn adjust_report_for_skips(
    delta: &mut BundleDelta,
    stored: &BTreeMap<String, String>,
    skipped: &[PathBuf],
) {
    for path in skipped {
        let key = path.to_string_lossy();
        if stored.contains_key(key.as_ref()) {
            delta.report.updated = delta.report.updated.saturating_sub(1);
        } else {
            delta.report.added = delta.report.added.saturating_sub(1);
        }
    }
}

/// Read the concept ids of the rows about to be removed, before the delete.
///
/// The change manifest ([`crate::catalog::audit::record_changes`]) names removed
/// concepts by their path-derived id, but [`delete_removed_concepts`] drops those
/// rows, so their ids must be captured first. Empty `removed_paths` short-circuits
/// with no query.
fn load_removed_concept_ids(
    bundle_id: i64,
    removed_paths: &[String],
) -> Result<Vec<String>, CatalogError> {
    if removed_paths.is_empty() {
        return Ok(Vec::new());
    }
    Spi::connect(|client| {
        let table = client
            .select(
                "SELECT id FROM pgokf.concepts WHERE bundle_id = $1 AND path = ANY($2)",
                None,
                &[bundle_id.into(), removed_paths.to_vec().into()],
            )
            .map_err(|error| spi_error("failed to load removed concept ids", &error))?;
        let mut ids = Vec::with_capacity(table.len());
        for row in table {
            ids.push(spi_read::required_column::<String>(
                &row,
                1,
                "failed to read removed concept id",
                "removed concept id is NULL",
            )?);
        }
        Ok(ids)
    })
}

/// Build the parallel `(concept_id, change_kind)` slices for the per-concept
/// change manifest of one sync.
///
/// Every staged concept was (re-)written, so it is classified `updated` when its
/// path was already stored before this sync and `added` otherwise; every id in
/// `removed_ids` is `removed`. The classification keys on the concept's own
/// normalized path against the pre-sync stored projection, so it matches the
/// paths the incremental diff itself compared.
fn build_change_manifest(
    stored: &BTreeMap<String, String>,
    staged: &[StagedConcept],
    removed_ids: &[String],
) -> (Vec<String>, Vec<String>) {
    let capacity = staged.len() + removed_ids.len();
    let mut concept_ids = Vec::with_capacity(capacity);
    let mut change_kinds = Vec::with_capacity(capacity);
    for entry in staged {
        let kind = if stored.contains_key(&entry.concept.path) {
            "updated"
        } else {
            "added"
        };
        concept_ids.push(entry.concept.id.clone());
        change_kinds.push(kind.to_owned());
    }
    for id in removed_ids {
        concept_ids.push(id.clone());
        change_kinds.push("removed".to_owned());
    }
    (concept_ids, change_kinds)
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
fn upsert_concepts(
    bundle_id: i64,
    staged: &[StagedConcept],
    text_search_config: &str,
) -> Result<(), CatalogError> {
    // Tags are the one ragged (per-row `text[]`) column, so they cannot be bound
    // as a single rectangular array. Instead the chunk's tags are flattened into
    // `$7` and each row carries an inclusive 1-based slice window
    // (`$12` lo, `$13` hi) into it; `($7)[lo:hi]` rebuilds the row's array (empty
    // when hi < lo). The inner subquery materializes that slice once so the
    // stored column and the weighted `body_tsv` share it.
    //
    // `$14` is the configured text-search regconfig (bound as text and cast in
    // SQL so no identifier is interpolated). All three weighted inputs (title,
    // metadata, body) are bounded with `left(...)` so their concatenated
    // `tsvector` cannot exceed the `MAXSTRPOS` limit and abort the sync — see
    // the `*_TSV_CHAR_LIMIT` constants.
    let upsert = format!(
        "
        INSERT INTO pgokf.concepts
            (bundle_id, tenant_id, id, path, type, title, description, tags, resource,
             body_text, file_hash, modified_at, body_tsv)
        SELECT
            $1, (SELECT b.tenant_id FROM pgokf.bundles b WHERE b.id = $1),
            t.id, t.path, t.type, t.title, t.description, t.tags, t.resource,
            t.body_text, t.file_hash, pg_catalog.to_timestamp(t.modified_at),
            pg_catalog.setweight(
                pg_catalog.to_tsvector($14::pg_catalog.regconfig,
                    pg_catalog.left(t.title, {TITLE_TSV_CHAR_LIMIT})), 'A')
            || pg_catalog.setweight(
                   pg_catalog.to_tsvector(
                       $14::pg_catalog.regconfig,
                       pg_catalog.left(
                           pg_catalog.concat_ws(' ',
                               pg_catalog.array_to_string(t.tags, ' '),
                               t.type, t.description),
                           {META_TSV_CHAR_LIMIT})),
                   'B')
            || pg_catalog.setweight(
                   pg_catalog.to_tsvector(
                       $14::pg_catalog.regconfig,
                       pg_catalog.left(t.body_text, {BODY_TSV_CHAR_LIMIT})),
                   'D')
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
            indexed_at = pg_catalog.now()"
    );

    for chunk in staged.chunks(BATCH_SIZE) {
        let columns = batch::marshal_concepts(chunk);
        Spi::run_with_args(
            &upsert,
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
                text_search_config.into(),
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
        INSERT INTO pgokf.concept_metadata (bundle_id, tenant_id, concept_id, key, value)
        SELECT $1,
               (SELECT b.tenant_id FROM pgokf.bundles b WHERE b.id = $1),
               d.concept_id, d.key, d.value::jsonb
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

/// Read the OKF format version declared in the bundle-root `index.md`.
///
/// The bundle-root `index.md` is a reserved OKF file (never a concept); its
/// frontmatter may carry an optional `okf_version`. This read is fully
/// defensive — an absent, oversized, unreadable, or malformed `index.md`, or an
/// absent/invalid `okf_version`, yields `None`, so it can never abort a sync.
/// Only the bundle-root `index.md` is consulted; nested `index.md` files carry
/// per-directory bookkeeping and never set the bundle version.
fn read_root_okf_version(root: &Path) -> Option<String> {
    let index_path = root.join("index.md");
    let metadata = std::fs::metadata(&index_path).ok()?;
    let max_file_bytes = u64::try_from(guc::max_file_bytes()).unwrap_or(u64::MAX);
    if !metadata.is_file() || metadata.len() > max_file_bytes {
        return None;
    }
    let bytes = std::fs::read(&index_path).ok()?;
    okf_parser::index_okf_version(&bytes, guc::max_frontmatter_bytes())
}

fn update_bundle_row(
    bundle_id: i64,
    sync_hash: &str,
    okf_version: Option<&str>,
) -> Result<(), CatalogError> {
    Spi::run_with_args(
        "UPDATE pgokf.bundles b
         SET file_count = (SELECT count(*)::integer
                           FROM pgokf.concepts c
                           WHERE c.bundle_id = b.id),
             last_synced_at = pg_catalog.now(),
             sync_hash = $2,
             okf_version = $3
         WHERE b.id = $1",
        &[bundle_id.into(), sync_hash.into(), okf_version.into()],
    )
    .map_err(|error| spi_error("failed to update bundle sync state", &error))
}

/// Apply the durable `okf_version_policy` to a bundle's declared OKF version.
///
/// An absent version (`None`) is always accepted unchanged. A *declared* but
/// unsupported version is either warned about and indexed anyway (`warn`, the
/// default — the value is still stored on `pgokf.bundles.okf_version`) or
/// rejected with SQLSTATE `22023`, aborting the sync (`reject`). A declared,
/// supported version passes silently.
fn apply_okf_version_policy(
    version: Option<String>,
    policy: &str,
) -> Result<Option<String>, CatalogError> {
    if let Some(declared) = &version
        && !is_supported_okf_version(declared)
    {
        if policy == "reject" {
            return Err(CatalogError::invalid_parameter(
                format!(
                    "bundle declares unsupported OKF version {declared} \
                     (this build supports 0.2 / 0.2.x); okf_version_policy=reject"
                ),
                Path::new(""),
            ));
        }
        pgrx::warning!(
            "pgokf: bundle declares unsupported OKF version {declared} \
             (this build supports 0.2 / 0.2.x); indexing anyway (okf_version_policy=warn)"
        );
    }
    Ok(version)
}

/// Emit the opt-in change notification for a completed sync.
///
/// Fires `pg_notify(<channel>, <json>)` with a JSON payload of the bundle id,
/// operation, and change counts. The channel and payload are bound as
/// parameters, never interpolated. Off by default (no channel configured), so
/// this is only called when `notify_channel` is set.
fn emit_change_notification(
    channel: &str,
    bundle_id: i64,
    op: SyncOp,
    report: &SyncReport,
) -> Result<(), CatalogError> {
    Spi::run_with_args(
        "SELECT pg_catalog.pg_notify($1, pg_catalog.jsonb_build_object(
             'bundle_id', $2::bigint,
             'op', $3::text,
             'added', $4::integer,
             'updated', $5::integer,
             'removed', $6::integer,
             'total', $7::integer)::text)",
        &[
            channel.into(),
            bundle_id.into(),
            op.as_str().into(),
            count_to_i32(report.added).into(),
            count_to_i32(report.updated).into(),
            count_to_i32(report.removed).into(),
            count_to_i32(report.total()).into(),
        ],
    )
    .map_err(|error| spi_error("failed to emit change notification", &error))
}

/// Run the ordered projection seam over the staged concepts.
///
/// Feature modules observe the finalized concept set here, in a fixed order:
/// the link-graph projection, then the bundle-wide link re-resolution (so the
/// inbound edges of *unchanged* concepts reflect targets added or removed during
/// this sync — only staged sources are re-projected, so their peers must be
/// re-evaluated against the post-upsert/delete concept set), then the provenance
/// projection, then the opt-in source-byte storage. The source-byte pass writes
/// nothing under the default `store_source`-off policy (every staged concept
/// carries `raw_content = None`); its order relative to links/provenance is
/// irrelevant since it only depends on the concept rows existing.
fn run_projection_seam(bundle_id: i64, staged: &[StagedConcept]) -> Result<(), CatalogError> {
    crate::catalog::links::project(bundle_id, staged)?;
    crate::catalog::links::reresolve_bundle(bundle_id)?;
    crate::catalog::provenance::project(bundle_id, staged)?;
    crate::catalog::source::project(bundle_id, staged)?;
    Ok(())
}

/// Project every reserved `log.md` in the current snapshot into
/// `pgokf.bundle_log`.
///
/// Reserved OKF files are never concepts, so a `log.md` never reaches the
/// staged set the projection seam consumes; this pass reconciles them
/// separately. It scans the whole current snapshot (which includes reserved
/// files), reads each `log.md` through the [`ByteSource`] — the same seam the
/// concept bytes come from — parses it into ordered entries, and hands the
/// per-directory entries to [`crate::catalog::bundle_log::project`], which
/// replaces the bundle's log rows wholesale so the projection tracks the files
/// (added, edited, and removed logs alike). It runs inside the sync transaction
/// after the concept projections. A `log.md` that cannot be *read* is warned
/// about and skipped so an unreadable auxiliary file can never abort a sync;
/// parsing itself is infallible.
fn project_bundle_logs<S: ByteSource>(
    bundle_id: i64,
    source: &S,
    current: &[FileMetadata],
) -> Result<(), CatalogError> {
    let mut directory_logs = Vec::new();
    for metadata in current {
        let path_text = metadata.path.to_string_lossy();
        if !is_reserved_log(&path_text) {
            continue;
        }
        let bytes = match source.read_bytes(&metadata.path) {
            Ok(bytes) => bytes,
            Err(error) => {
                pgrx::warning!(
                    "pgokf: skipping unreadable reserved log {}: {error}",
                    metadata.path.display()
                );
                continue;
            }
        };
        let directory = parent_directory(&path_text).to_owned();
        directory_logs.push((directory, crate::catalog::bundle_log::parse_log(&bytes)));
    }
    crate::catalog::bundle_log::project(bundle_id, &directory_logs)
}

/// The shared sync engine used by `register_bundle`, `refresh_bundle`, and
/// `register_bundle_content`; see the module docs for the full step list.
///
/// Generic over the [`ByteSource`] seam so the classify/parse/upsert/project
/// pipeline is identical whether the bytes come from an on-disk
/// [`FilesystemSource`] or an in-memory [`ContentSource`]. Callers must already
/// hold the bundle advisory lock and have authorized the operation.
///
/// `op` and `bundle_path` identify the operation for the audit trail and the
/// change notification appended at the successful tail. Both the
/// `pgokf_private.sync_log` row and the `pg_notify` announcement commit
/// atomically with the sync transaction, so a logged row (or a delivered
/// notification, on commit) always corresponds to a committed operation.
pub(crate) fn run_bundle_sync<S: ByteSource>(
    bundle_id: i64,
    source: &S,
    op: SyncOp,
    bundle_path: &str,
) -> Result<SyncReport, CatalogError> {
    let defaults = crate::catalog::config::sync_defaults()?;
    let stored = load_stored_hashes(bundle_id)?;
    let current = source.snapshot(&defaults.exclude)?;
    let mut delta = classify_changes(&stored, &current);
    let outcome = stage_changed_concepts(source, &delta, defaults.strict, defaults.store_source)?;
    // Keep the report honest: files skipped under the non-strict policy were
    // classified but never written.
    adjust_report_for_skips(&mut delta, &stored, &outcome.skipped);
    let staged = outcome.staged;

    // Capture the removed concepts' ids for the change manifest before the
    // delete drops the rows they are read from.
    let removed_ids = load_removed_concept_ids(bundle_id, &delta.removed_paths)?;

    delete_removed_concepts(bundle_id, &delta.removed_paths)?;
    upsert_concepts(bundle_id, &staged, &defaults.text_search_config)?;
    replace_concept_metadata(bundle_id, &staged)?;

    // Ordered projection seam: feature modules observe the staged concepts
    // here (link graph and its bundle-wide re-resolution, provenance, then
    // opt-in source-byte storage).
    run_projection_seam(bundle_id, &staged)?;

    // Reserved log.md activity logs are not concepts, so they are reconciled
    // separately from the whole snapshot (which includes reserved files),
    // reading each through the same ByteSource.
    project_bundle_logs(bundle_id, source, &current)?;

    // Apply the OKF-version conformance policy before finalizing the bundle
    // row: under `reject` an unsupported declared version aborts the sync here,
    // so a rejected bundle is never recorded as synced.
    let okf_version =
        apply_okf_version_policy(source.root_okf_version(), &defaults.okf_version_policy)?;
    let sync_hash = bundle_sync_hash(&current);
    update_bundle_row(bundle_id, &sync_hash, okf_version.as_deref())?;

    // Audit trail: append exactly one row for this operation and prune history
    // to the retention policy — the mechanism that activates
    // sync_log_retention_days. Commits atomically with the sync.
    let sync_id = crate::catalog::audit::record(
        bundle_id,
        bundle_path,
        op.as_str(),
        Some(&delta.report),
        Some(&sync_hash),
        defaults.sync_log_retention_days,
    )?;

    // Per-concept change manifest: hang the concrete added/updated/removed
    // concepts off the audit row so an operator can see what this sync changed.
    let (change_ids, change_kinds) = build_change_manifest(&stored, &staged, &removed_ids);
    crate::catalog::audit::record_changes(sync_id, bundle_id, &change_ids, &change_kinds)?;

    // Opt-in change notification (LISTEN/NOTIFY). Zero overhead when the
    // notify_channel key is empty (no channel resolved).
    if let Some(channel) = &defaults.notify_channel {
        emit_change_notification(channel, bundle_id, op, &delta.report)?;
    }

    Ok(delta.report)
}

fn lookup_bundle_id_by_path(canonical_path: &str) -> Result<Option<i64>, CatalogError> {
    Spi::connect(|client| {
        // Scope the duplicate check to the current tenant: the registration key
        // is UNIQUE (tenant_id, path), so a different tenant may register the same
        // path and must not be seen as a duplicate here.
        let table = client
            .select(
                "SELECT id FROM pgokf.bundles
                 WHERE tenant_id = pgokf_private.effective_tenant() AND path = $1",
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
    // The bundle row is stamped with the session's effective tenant; every child
    // row then inherits this bundle's tenant_id (single-tenant bundle).
    Spi::get_one_with_args::<i64>(
        "INSERT INTO pgokf.bundles (tenant_id, path, name, options)
         VALUES (pgokf_private.effective_tenant(), $1, $2, COALESCE($3, '{}'::jsonb))
         RETURNING id",
        &[canonical_path.into(), name.into(), options.into()],
    )
    .map_err(|error| spi_error("failed to insert bundle row", &error))?
    .ok_or_else(|| CatalogError::internal("bundle insert returned no id", Path::new("")))
}

fn lookup_bundle_path_and_type(bundle_id: i64) -> Result<Option<(String, String)>, CatalogError> {
    Spi::connect(|client| {
        let table = client
            .select(
                "SELECT path, source_type FROM pgokf.bundles WHERE id = $1",
                Some(1),
                &[bundle_id.into()],
            )
            .map_err(|error| spi_error("failed to look up bundle path", &error))?;
        let Some(row) = table.into_iter().next() else {
            return Ok(None);
        };
        let path: String = spi_read::required_column(
            &row,
            1,
            "failed to read bundle path",
            "bundle path is NULL",
        )?;
        let source_type: String = spi_read::required_column(
            &row,
            2,
            "failed to read bundle source_type",
            "bundle source_type is NULL",
        )?;
        Ok(Some((path, source_type)))
    })
}

fn register_bundle_impl(
    path: &str,
    name: Option<String>,
    options: Option<pgrx::JsonB>,
) -> Result<(i64, String, SyncReport), CatalogError> {
    security::authorize_current_user(security::Operation::Ingest, Path::new(""))?;
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
    let report = run_bundle_sync(
        bundle_id,
        &FilesystemSource::new(canonical_root),
        SyncOp::Register,
        &canonical_text,
    )?;
    Ok((bundle_id, canonical_text, report))
}

fn refresh_bundle_impl(bundle_id: i64) -> Result<(String, SyncReport), CatalogError> {
    security::authorize_current_user(security::Operation::Ingest, Path::new(""))?;
    // Write-side tenant confinement: when pgokf.tenant is set, a bundle owned by
    // another tenant (or absent) is rejected as an unknown bundle before any
    // lookup, lock, or filesystem access — mirroring the read-side RLS policy.
    security::enforce_bundle_tenant(bundle_id)?;
    let (stored_path, source_type) = lookup_bundle_path_and_type(bundle_id)?.ok_or_else(|| {
        CatalogError::invalid_parameter(
            format!("bundle {bundle_id} is not registered"),
            Path::new(""),
        )
    })?;

    // A content-sourced bundle has no filesystem root to refresh from: its
    // bytes live only in whatever caller supplied them. Re-syncing it means
    // calling register_bundle_content again with the current content, so
    // refreshing it from disk is a caller error (22023).
    if source_type == "content" {
        return Err(CatalogError::invalid_parameter(
            format!(
                "bundle {bundle_id} is content-sourced; content bundles are \
                 re-synced by calling pgokf.register_bundle_content, not \
                 pgokf.refresh_bundle"
            ),
            Path::new(""),
        ));
    }

    // Lock on the stored canonical path so refresh serializes with a
    // concurrent register/refresh of the same bundle, then re-validate the
    // root before touching the filesystem.
    acquire_bundle_lock(&stored_path)?;
    let canonical_root = resolve_bundle_root(&stored_path)?;
    let report = run_bundle_sync(
        bundle_id,
        &FilesystemSource::new(canonical_root),
        SyncOp::Refresh,
        &stored_path,
    )?;
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
    /// Requires membership in `pgokf_writer` (an admin qualifies by
    /// inheritance). The path must be absolute, traversal-free, and
    /// canonicalizable; the canonical path is stored and must not already be
    /// registered (SQLSTATE `23505` otherwise — use `pgokf.refresh_bundle` to
    /// re-synchronize).
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
    /// Requires membership in `pgokf_writer` (an admin qualifies by
    /// inheritance). Only files whose BLAKE3 content hash changed are
    /// re-parsed; unchanged rows are left untouched (preserving `indexed_at`),
    /// and rows for deleted files are removed.
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
GRANT EXECUTE ON FUNCTION pgokf.register_bundle(text, text, jsonb) TO pgokf_writer;
GRANT EXECUTE ON FUNCTION pgokf.refresh_bundle(bigint) TO pgokf_writer;
COMMENT ON FUNCTION pgokf.register_bundle(text, text, jsonb) IS
    'Register an OKF bundle root and synchronize it into the catalog. Writer-tier (pgokf_writer; admin inherits it); raises 23505 if the canonical path is already registered.';
COMMENT ON FUNCTION pgokf.refresh_bundle(bigint) IS
    'Incrementally re-synchronize a registered bundle: re-parses only content-changed files, removes rows for deleted files. Writer-tier (pgokf_writer; admin inherits it).';
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

        fn snapshot(&self) -> Vec<FileMetadata> {
            discover(&SyncConfig::new(&self.root))
                .expect("discover temp bundle")
                .files()
                .values()
                .cloned()
                .collect()
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
    fn combined_tsv_char_limits_keep_worst_case_under_the_tsvector_bound() {
        // Arrange: Postgres caps a tsvector (pool + position data) at MAXSTRPOS
        // bytes. The limit applies to the CONCATENATED A||B||D vector, and in the
        // worst case (single four-byte lexemes) the size is bounded by four times
        // the total input characters — so all three bounds must sum safely under
        // the limit, not each individually.
        const MAX_TSVECTOR_STRPOS_BYTES: i64 = (1 << 20) - 1; // 1_048_575
        const MAX_UTF8_BYTES_PER_CHAR: i64 = 4;

        // Act: worst-case byte length of the three concatenated tsvector inputs.
        let total_chars = i64::from(TITLE_TSV_CHAR_LIMIT)
            + i64::from(META_TSV_CHAR_LIMIT)
            + i64::from(BODY_TSV_CHAR_LIMIT);
        let worst_case_bytes = total_chars * MAX_UTF8_BYTES_PER_CHAR;

        // Assert: the combined worst case stays under the 54000 threshold with
        // headroom, so no in-byte-limit document can abort a sync.
        assert!(worst_case_bytes < MAX_TSVECTOR_STRPOS_BYTES);
        assert!(
            worst_case_bytes <= MAX_TSVECTOR_STRPOS_BYTES * 9 / 10,
            "want >=10% headroom below MAXSTRPOS, got {worst_case_bytes} bytes"
        );
    }

    #[test]
    fn adjust_report_for_skips_decrements_added_for_a_new_file() {
        // Arrange: a skipped file with no stored hash was classified as added.
        let mut delta = BundleDelta {
            report: SyncReport {
                added: 3,
                updated: 1,
                removed: 0,
                unchanged: 0,
            },
            ..Default::default()
        };
        let stored = BTreeMap::new();
        let skipped = vec![PathBuf::from("brand-new.md")];

        // Act
        adjust_report_for_skips(&mut delta, &stored, &skipped);

        // Assert
        assert_eq!(delta.report.added, 2);
        assert_eq!(delta.report.updated, 1);
    }

    #[test]
    fn adjust_report_for_skips_decrements_updated_for_a_known_file() {
        // Arrange: a skipped file with a stored hash was classified as updated.
        let mut delta = BundleDelta {
            report: SyncReport {
                added: 2,
                updated: 2,
                removed: 0,
                unchanged: 0,
            },
            ..Default::default()
        };
        let stored = BTreeMap::from([("known.md".to_owned(), hash_bytes(b"old"))]);
        let skipped = vec![PathBuf::from("known.md")];

        // Act
        adjust_report_for_skips(&mut delta, &stored, &skipped);

        // Assert
        assert_eq!(delta.report.updated, 1);
        assert_eq!(delta.report.added, 2);
    }

    #[test]
    fn adjust_report_for_skips_saturates_at_zero() {
        // Arrange: more skips than counted (defensive; never happens in sync).
        let mut delta = BundleDelta {
            report: SyncReport {
                added: 0,
                updated: 0,
                removed: 0,
                unchanged: 0,
            },
            ..Default::default()
        };
        let stored = BTreeMap::new();
        let skipped = vec![PathBuf::from("a.md"), PathBuf::from("b.md")];

        // Act
        adjust_report_for_skips(&mut delta, &stored, &skipped);

        // Assert
        assert_eq!(delta.report.added, 0);
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
