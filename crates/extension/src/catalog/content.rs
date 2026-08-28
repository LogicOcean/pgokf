//! Mountless content ingestion: `pgokf.register_bundle_content`.
//!
//! # Why this exists
//!
//! The filesystem ingestion path ([`crate::catalog::sync`]) requires the OKF
//! bundle to be *readable from the `PostgreSQL` backend's filesystem* — a POSIX
//! mount over the object store / data lake. This module adds the **mountless**
//! alternative: a standalone companion process (see the `pgokf-ingest` crate)
//! reads an S3-compatible store over the network, then streams the collected
//! `(path, bytes)` into `PostgreSQL` through this function. The extension itself
//! never performs any network I/O — the companion holds the object-store
//! credentials, and `PostgreSQL` only ever sees bytes it was handed.
//!
//! # Shared pipeline, in-memory source
//!
//! Ingestion reuses the exact classify/parse/upsert/project pipeline the
//! filesystem path uses, through the [`crate::catalog::sync::ByteSource`] seam:
//! this function validates and wraps the caller-supplied arrays in a
//! [`crate::catalog::sync::ContentSource`] and calls
//! [`crate::catalog::sync::run_bundle_sync`]. A content bundle is therefore
//! diffed against its stored projection exactly like a filesystem bundle — a
//! second call with changed content upserts the changed concepts and deletes the
//! ones no longer present — so `register_bundle_content` is a create-or-resync.
//!
//! # Identity and hardening
//!
//! A content bundle is keyed on the synthetic path `content:<name>` stored in
//! `pgokf.bundles.path` (which is `UNIQUE` and can never collide with an
//! absolute filesystem path) with `source_type = 'content'`. The bundle
//! advisory lock is taken on that key, so a content resync serializes with any
//! other operation on the same bundle. The function is writer-tier
//! ([`crate::security::Operation::Ingest`], the ingest account the companion
//! authenticates as), `SECURITY DEFINER` with a pinned `search_path`, and its
//! `EXECUTE` is revoked from `PUBLIC` and granted to `pgokf_writer`.

use std::path::{Component, Path};

use pgrx::Spi;

use crate::catalog::sync::{self, ContentSource};
use crate::errors::CatalogError;
use crate::guc;
use crate::security;

/// The synthetic-path prefix under which content bundles are keyed in
/// `pgokf.bundles.path`. An absolute filesystem path can never begin with it,
/// so the two namespaces cannot collide on the `UNIQUE` path column.
const CONTENT_PATH_PREFIX: &str = "content:";

fn spi_error(context: &str, error: &pgrx::spi::Error) -> CatalogError {
    CatalogError::internal(format!("{context}: {error}"), Path::new(""))
}

/// Reject any provided path that is not a safe bundle-relative path.
///
/// Mirrors the traversal defenses applied to on-disk paths: no absolute path,
/// no `..` traversal, no `\0`, and non-empty. Reserved OKF files (`index.md` /
/// `log.md`) are *allowed through* here exactly as the filesystem scan surfaces
/// them — the shared pipeline skips them as concepts, and a root `index.md`
/// still contributes the bundle `okf_version`. Raises SQLSTATE `22023`.
fn validate_content_path(path: &str) -> Result<(), CatalogError> {
    if path.is_empty() {
        return Err(CatalogError::invalid_parameter(
            "content path must not be empty",
            Path::new(""),
        ));
    }
    if path.contains('\0') {
        return Err(CatalogError::invalid_parameter(
            "content path must not contain NUL bytes",
            Path::new(""),
        ));
    }
    let unsafe_path = Path::new(path).components().any(|component| {
        matches!(
            component,
            Component::ParentDir | Component::RootDir | Component::Prefix(_)
        )
    });
    if unsafe_path {
        return Err(CatalogError::invalid_parameter(
            format!("content path is not a safe bundle-relative path: {path}"),
            Path::new(path),
        ));
    }
    Ok(())
}

/// Validate the caller-supplied arrays.
///
/// Enforces the contract from the SQL surface: the two arrays are of the same
/// length, every path is a safe bundle-relative path, and the
/// `pgokf.max_bundle_files` / `pgokf.max_file_bytes` ceilings are honored on the
/// provided content. Every violation is SQLSTATE `22023`. A NULL element in
/// either array is refused by pgrx's `text[]` / `bytea[]` unboxing before this
/// runs, so a stored concept never carries a NULL path or NULL bytes.
fn validate_arrays(paths: &[String], contents: &[Vec<u8>]) -> Result<(), CatalogError> {
    if paths.len() != contents.len() {
        return Err(CatalogError::invalid_parameter(
            format!(
                "paths and contents must have the same length (got {} paths, {} contents)",
                paths.len(),
                contents.len()
            ),
            Path::new(""),
        ));
    }

    let max_files = guc::max_bundle_files();
    if paths.len() > max_files {
        return Err(CatalogError::invalid_parameter(
            format!(
                "content bundle has {} files, exceeding the pgokf.max_bundle_files ceiling of {max_files}",
                paths.len()
            ),
            Path::new(""),
        ));
    }

    let max_bytes = guc::max_file_bytes();
    for (path, content) in paths.iter().zip(contents) {
        validate_content_path(path)?;
        if content.len() > max_bytes {
            return Err(CatalogError::invalid_parameter(
                format!(
                    "content for {path} is {} bytes, exceeding the pgokf.max_file_bytes ceiling of {max_bytes}",
                    content.len()
                ),
                Path::new(path),
            ));
        }
    }

    Ok(())
}

/// Copy every `bytea[]` element into an owned buffer, rejecting a NULL element
/// with SQLSTATE `22023`.
///
/// A `bytea[]` element is unboxed as a borrowed `&[u8]` into the call's datum;
/// each is copied to an owned `Vec<u8>` so the [`ContentSource`] can outlive the
/// borrow and (under `store_source`) persist the exact bytes. A NULL element
/// would otherwise become a concept with no bytes, so it is refused up front.
// The `bytea[]` datum arrives by value from the pg_extern wrapper and must be
// owned here (its borrowed elements cannot outlive it), so taking it by value
// and iterating by reference is correct even though the value is not moved.
#[allow(clippy::needless_pass_by_value)]
fn collect_contents(contents: pgrx::Array<'_, &[u8]>) -> Result<Vec<Vec<u8>>, CatalogError> {
    let mut owned = Vec::with_capacity(contents.len());
    for (index, element) in contents.iter().enumerate() {
        let bytes = element.ok_or_else(|| {
            CatalogError::invalid_parameter(
                format!("contents must not contain NULL elements (index {index})"),
                Path::new(""),
            )
        })?;
        owned.push(bytes.to_vec());
    }
    Ok(owned)
}

/// The synthetic bundle key for a content bundle of the given name.
fn content_path_key(name: &str) -> String {
    format!("{CONTENT_PATH_PREFIX}{name}")
}

/// Look up an existing content bundle by its synthetic key, returning its id.
fn lookup_content_bundle(path_key: &str) -> Result<Option<i64>, CatalogError> {
    Spi::connect(|client| {
        let table = client
            .select(
                "SELECT id FROM pgokf.bundles WHERE path = $1",
                Some(1),
                &[path_key.into()],
            )
            .map_err(|error| spi_error("failed to look up content bundle", &error))?;
        if table.is_empty() {
            return Ok(None);
        }
        table
            .first()
            .get_one::<i64>()
            .map_err(|error| spi_error("failed to read content bundle id", &error))
    })
}

/// Insert a new content bundle row and return its assigned id.
fn insert_content_bundle(
    path_key: &str,
    name: &str,
    options: Option<pgrx::JsonB>,
) -> Result<i64, CatalogError> {
    Spi::get_one_with_args::<i64>(
        "INSERT INTO pgokf.bundles (path, name, options, source_type)
         VALUES ($1, $2, COALESCE($3, '{}'::jsonb), 'content')
         RETURNING id",
        &[path_key.into(), name.into(), options.into()],
    )
    .map_err(|error| spi_error("failed to insert content bundle row", &error))?
    .ok_or_else(|| CatalogError::internal("content bundle insert returned no id", Path::new("")))
}

/// Authorize, validate, and run the shared sync pipeline against an in-memory
/// [`ContentSource`], creating the content bundle or resyncing an existing one.
fn register_bundle_content_impl(
    name: &str,
    paths: Vec<String>,
    contents: Vec<Vec<u8>>,
    options: Option<pgrx::JsonB>,
) -> Result<(i64, String, okf_sync::SyncReport), CatalogError> {
    security::authorize_current_user(security::Operation::Ingest, Path::new(""))?;

    validate_arrays(&paths, &contents)?;
    let source = ContentSource::new(paths, contents);

    let path_key = content_path_key(name);
    // Serialize on the synthetic bundle key so a content resync cannot race
    // another operation on the same bundle.
    sync::acquire_bundle_lock(&path_key)?;

    let bundle_id = match lookup_content_bundle(&path_key)? {
        Some(existing) => existing,
        None => insert_content_bundle(&path_key, name, options)?,
    };

    let report = sync::run_bundle_sync(bundle_id, &source)?;
    Ok((bundle_id, path_key, report))
}

/// SQL-facing content-ingestion entry point, installed into the `pgokf` schema.
#[pgrx::pg_schema]
mod pgokf {
    use pgrx::{default, extension_sql, pg_extern};

    use super::register_bundle_content_impl;
    use crate::catalog::types;

    /// Register (or resync) an OKF bundle from in-memory content.
    ///
    /// Requires membership in `pgokf_writer` (an admin qualifies by
    /// inheritance). `paths` and `contents` must be non-NULL arrays of equal
    /// length with no NULL elements; each path must be a safe bundle-relative
    /// path (no absolute path, no `..`, no NUL). The bundle is keyed on the
    /// synthetic path `content:<name>`, so calling it again with new content
    /// diffs against the stored projection: changed concepts are upserted and
    /// concepts no longer present are removed (a create-or-resync). The
    /// `pgokf.max_bundle_files` and `pgokf.max_file_bytes` ceilings apply to the
    /// provided content. Raises SQLSTATE `22023` on a shape/path violation and
    /// `42501` for a caller outside `pgokf_writer`.
    #[pg_extern(requires = ["catalog_tables"])]
    fn register_bundle_content(
        name: &str,
        paths: Vec<String>,
        contents: pgrx::Array<'_, &[u8]>,
        options: default!(Option<pgrx::JsonB>, "'{}'"),
    ) -> pgrx::composite_type!('static, "pgokf.bundle_sync_result") {
        // pgrx unboxes each `bytea[]` element as a borrowed `&[u8]` into the
        // call's datum; copy each to an owned buffer (rejecting NULL elements)
        // so the in-memory source can hold and, under store_source, persist the
        // exact bytes.
        let contents = super::collect_contents(contents).unwrap_or_else(|error| error.raise());
        let (bundle_id, path_key, report) =
            register_bundle_content_impl(name, paths, contents, options)
                .unwrap_or_else(|error| error.raise());
        types::bundle_sync_result(bundle_id, &path_key, report)
            .unwrap_or_else(|error| error.raise())
    }

    extension_sql!(
        r"
ALTER FUNCTION pgokf.register_bundle_content(text, text[], bytea[], jsonb)
    SECURITY DEFINER SET search_path = pg_catalog, pg_temp;
REVOKE ALL ON FUNCTION pgokf.register_bundle_content(text, text[], bytea[], jsonb) FROM PUBLIC;
GRANT EXECUTE ON FUNCTION pgokf.register_bundle_content(text, text[], bytea[], jsonb) TO pgokf_writer;
COMMENT ON FUNCTION pgokf.register_bundle_content(text, text[], bytea[], jsonb) IS
    'Register or resync an OKF bundle from in-memory content: the mountless ingestion path a companion process uses to stream bytes read from an object store, so the extension performs no network I/O. paths[] and contents[] must be equal-length, non-null arrays of safe bundle-relative paths and their bytes; the bundle is keyed on content:<name> with source_type=''content'' and re-called to resync (changed concepts upserted, missing ones deleted). Writer-tier (pgokf_writer; admin inherits it). Raises 22023 on a shape/path violation, honoring the max_bundle_files/max_file_bytes ceilings.';
",
        name = "content_function_hardening",
        requires = [register_bundle_content]
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::errors::ErrorKind;

    #[test]
    fn validate_content_path_accepts_nested_and_reserved_paths() {
        // Arrange / Act / Assert: ordinary and reserved bundle-relative paths
        // pass — a root index.md is allowed through so it can set okf_version.
        assert!(validate_content_path("alpha.md").is_ok());
        assert!(validate_content_path("nested/beta.md").is_ok());
        assert!(validate_content_path("index.md").is_ok());
    }

    #[test]
    fn validate_content_path_rejects_absolute_paths() {
        // Arrange / Act
        let error =
            validate_content_path("/etc/passwd").expect_err("absolute paths must be rejected");

        // Assert
        assert_eq!(error.kind(), ErrorKind::InvalidParameter);
        assert_eq!(error.sqlstate(), "22023");
    }

    #[test]
    fn validate_content_path_rejects_parent_traversal() {
        // Arrange / Act
        let error =
            validate_content_path("../escape.md").expect_err("parent traversal must be rejected");

        // Assert
        assert_eq!(error.sqlstate(), "22023");
    }

    #[test]
    fn validate_content_path_rejects_empty_path() {
        // Arrange / Act
        let error = validate_content_path("").expect_err("empty paths must be rejected");

        // Assert
        assert_eq!(error.sqlstate(), "22023");
    }

    #[test]
    fn validate_arrays_rejects_length_mismatch() {
        // Arrange: two paths but one content.
        let paths = vec!["a.md".to_owned(), "b.md".to_owned()];
        let contents = vec![b"only one".to_vec()];

        // Act
        let error =
            validate_arrays(&paths, &contents).expect_err("unequal lengths must be rejected");

        // Assert
        assert_eq!(error.sqlstate(), "22023");
    }

    #[test]
    fn content_path_key_is_prefixed_and_cannot_collide_with_a_filesystem_path() {
        // Arrange / Act
        let key = content_path_key("handbook");

        // Assert: the synthetic key is prefixed and never absolute, so it can
        // never collide with a canonical filesystem bundle path.
        assert_eq!(key, "content:handbook");
        assert!(!key.starts_with('/'));
    }
}
