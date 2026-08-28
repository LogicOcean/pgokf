//! Path-containment and role-authorization policy for catalog operations.
//!
//! Filesystem access is only permitted under explicitly configured allowed
//! roots, with both sides canonicalized so symlinks cannot escape
//! containment. Authorization is expressed against the [`RoleMembership`]
//! abstraction so the policy in [`authorize`] is unit-testable without a
//! running `PostgreSQL` server; [`PostgresRoleMembership`] is the production
//! implementation backed by `pg_has_role`.

use crate::errors::CatalogError;
use std::path::{Component, Path, PathBuf};

/// Highest tier: configuration writes and file-writing exports.
pub const PGOKF_ADMIN_ROLE: &str = "pgokf_admin";
/// Middle tier: bundle ingestion (register / refresh / unregister).
pub const PGOKF_WRITER_ROLE: &str = "pgokf_writer";
/// Lowest tier: read-only search and catalog reads.
pub const PGOKF_READER_ROLE: &str = "pgokf_reader";

/// Catalog operations subject to role-based authorization.
///
/// Each operation names the least-privilege tier it requires, and the tiers
/// are strictly nested — `Search` (reader) < `Ingest` (writer) <
/// `Register` (admin). Because the roles inherit downward (`pgokf_admin`
/// is granted `pgokf_writer`, which is granted `pgokf_reader`), a higher tier
/// satisfies any lower requirement; [`authorize`] encodes that by accepting
/// the required role *or any higher one*.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Operation {
    /// Administrative mutation — configuration writes (`set_config`,
    /// `reset_config`) and the file-writing exports (`export_parquet`,
    /// `export_sources`). Requires `pgokf_admin`.
    ///
    /// (The variant is named for the original bundle-registration gate; bundle
    /// ingestion has since moved to the lower-privilege [`Operation::Ingest`]
    /// tier, and this variant now guards only the admin-only surface.)
    Register,
    /// Bundle ingestion — register, refresh, or unregister a bundle. Requires
    /// `pgokf_writer`; an admin satisfies it by inheritance.
    Ingest,
    /// Query the catalog. Requires `pgokf_reader`; a writer or admin satisfies
    /// it by inheritance.
    Search,
}

/// Abstract role lookup so authorization policy can be tested without a
/// running `PostgreSQL` server.
pub trait RoleMembership {
    /// Report whether the current user is a member of `role`.
    ///
    /// # Errors
    ///
    /// Returns a [`CatalogError`] when membership cannot be determined, for
    /// example because the underlying lookup query failed.
    fn is_member_of(&self, role: &str) -> Result<bool, CatalogError>;
}

/// Reject unsafe path syntax before any filesystem access.
///
/// # Errors
///
/// Returns an [`crate::errors::ErrorKind::InvalidParameter`] error when the
/// path is relative, contains NUL bytes, or contains `..` components.
pub fn validate_path_syntax(path: &Path, bundle_relative_path: &Path) -> Result<(), CatalogError> {
    if !path.is_absolute() {
        return Err(CatalogError::invalid_parameter(
            format!("path must be absolute: {}", path.display()),
            bundle_relative_path,
        ));
    }

    if path.as_os_str().to_string_lossy().contains('\0') {
        return Err(CatalogError::invalid_parameter(
            "path must not contain NUL bytes",
            bundle_relative_path,
        ));
    }

    if path.components().any(|part| part == Component::ParentDir) {
        return Err(CatalogError::invalid_parameter(
            format!("path traversal is not allowed: {}", path.display()),
            bundle_relative_path,
        ));
    }

    Ok(())
}

/// Canonicalize and validate configured roots, deduplicating resolved paths.
///
/// # Errors
///
/// Returns an [`crate::errors::ErrorKind::InvalidParameter`] error when
/// `allowed_roots` is empty, a root has unsafe syntax, a root cannot be
/// canonicalized, or a resolved root is not a directory.
pub fn validate_allowed_roots(
    allowed_roots: &[PathBuf],
    bundle_relative_path: &Path,
) -> Result<Vec<PathBuf>, CatalogError> {
    if allowed_roots.is_empty() {
        return Err(CatalogError::invalid_parameter(
            "allowed_roots must contain at least one root",
            bundle_relative_path,
        ));
    }

    let mut canonical_roots = Vec::with_capacity(allowed_roots.len());
    for root in allowed_roots {
        validate_path_syntax(root, bundle_relative_path)?;
        let canonical = std::fs::canonicalize(root).map_err(|error| {
            CatalogError::invalid_parameter(
                format!(
                    "failed to canonicalize allowed root {}: {error}",
                    root.display()
                ),
                bundle_relative_path,
            )
        })?;
        if !canonical.is_dir() {
            return Err(CatalogError::invalid_parameter(
                format!("allowed root is not a directory: {}", root.display()),
                bundle_relative_path,
            ));
        }
        if !canonical_roots.contains(&canonical) {
            canonical_roots.push(canonical);
        }
    }

    Ok(canonical_roots)
}

/// Canonicalize `path` and ensure its resolved target remains under one of the
/// canonical allowed roots. Resolving both sides prevents symlinks from
/// escaping containment.
///
/// # Errors
///
/// Returns an [`crate::errors::ErrorKind::InvalidParameter`] error when the
/// path or roots fail validation, when canonicalization fails, or when the
/// resolved path escapes every allowed root.
pub fn canonicalize_contained_path(
    path: &Path,
    allowed_roots: &[PathBuf],
    bundle_relative_path: &Path,
) -> Result<PathBuf, CatalogError> {
    validate_path_syntax(path, bundle_relative_path)?;
    let roots = validate_allowed_roots(allowed_roots, bundle_relative_path)?;
    let canonical_path = std::fs::canonicalize(path).map_err(|error| {
        CatalogError::invalid_parameter(
            format!("failed to canonicalize path {}: {error}", path.display()),
            bundle_relative_path,
        )
    })?;

    if roots.iter().any(|root| canonical_path.starts_with(root)) {
        Ok(canonical_path)
    } else {
        Err(CatalogError::invalid_parameter(
            format!(
                "resolved path {} is outside allowed_roots",
                canonical_path.display()
            ),
            bundle_relative_path,
        ))
    }
}

/// The roles that satisfy an operation, in the order they are checked: the
/// minimum tier first, then every higher tier that inherits it.
///
/// Listing the higher tiers makes the policy independent of `pg_has_role`'s
/// own inheritance resolution — an admin is authorized for an `Ingest`
/// operation even if the role-grant chain were ever misconfigured — and keeps
/// the tier hierarchy explicit and unit-testable.
fn satisfying_roles(operation: Operation) -> &'static [&'static str] {
    match operation {
        Operation::Search => &[PGOKF_READER_ROLE, PGOKF_WRITER_ROLE, PGOKF_ADMIN_ROLE],
        Operation::Ingest => &[PGOKF_WRITER_ROLE, PGOKF_ADMIN_ROLE],
        Operation::Register => &[PGOKF_ADMIN_ROLE],
    }
}

/// The least-privilege role an operation requires, used only for the denial
/// message.
fn minimum_role(operation: Operation) -> &'static str {
    match operation {
        Operation::Search => PGOKF_READER_ROLE,
        Operation::Ingest => PGOKF_WRITER_ROLE,
        Operation::Register => PGOKF_ADMIN_ROLE,
    }
}

/// Enforce operation-specific role policy.
///
/// `Register` requires `pgokf_admin`; `Ingest` requires `pgokf_writer`;
/// `Search` requires `pgokf_reader`. Because the tiers inherit downward, a
/// higher tier satisfies any lower requirement — the check accepts the
/// required role or any higher one and short-circuits on the first match.
///
/// # Errors
///
/// Returns an [`crate::errors::ErrorKind::InsufficientPrivilege`] error when
/// the user is a member of none of the satisfying roles, or propagates the
/// failure reported by the [`RoleMembership`] implementation with the caller's
/// `bundle_relative_path` attached.
pub fn authorize<R: RoleMembership>(
    operation: Operation,
    roles: &R,
    bundle_relative_path: &Path,
) -> Result<(), CatalogError> {
    for role in satisfying_roles(operation) {
        let is_member = roles.is_member_of(role).map_err(|error| {
            CatalogError::new(error.kind(), error.message(), bundle_relative_path)
        })?;
        if is_member {
            return Ok(());
        }
    }

    Err(CatalogError::insufficient_privilege(
        format!(
            "operation requires membership in {}",
            minimum_role(operation)
        ),
        bundle_relative_path,
    ))
}

/// [`RoleMembership`] backed by `PostgreSQL`'s `pg_has_role`, evaluated for the
/// **session user** via SPI.
///
/// Membership is checked against `session_user` rather than `current_user` so
/// the policy stays correct inside `SECURITY DEFINER` functions, where
/// `current_user` is the function owner instead of the caller. `session_user`
/// still identifies the invoking session there, and a session can only
/// `SET ROLE` to a role it is a member of, so this never widens access.
/// Superusers pass `pg_has_role` for every role, as everywhere else in
/// `PostgreSQL`.
///
/// The lookup runs through read-only SPI (`SpiClient::select`) so it is safe
/// to call from `STABLE`, `PARALLEL SAFE` functions such as
/// `pgokf.concept_search`; a writable SPI call would try to assign a
/// transaction ID, which is an error inside a parallel worker.
pub struct PostgresRoleMembership;

impl RoleMembership for PostgresRoleMembership {
    fn is_member_of(&self, role: &str) -> Result<bool, CatalogError> {
        let query = match role {
            PGOKF_ADMIN_ROLE => {
                "SELECT pg_catalog.pg_has_role(session_user, 'pgokf_admin', 'MEMBER')"
            }
            PGOKF_WRITER_ROLE => {
                "SELECT pg_catalog.pg_has_role(session_user, 'pgokf_writer', 'MEMBER')"
            }
            PGOKF_READER_ROLE => {
                "SELECT pg_catalog.pg_has_role(session_user, 'pgokf_reader', 'MEMBER')"
            }
            _ => {
                return Err(CatalogError::internal(
                    format!("unknown authorization role: {role}"),
                    Path::new(""),
                ));
            }
        };

        pgrx::Spi::connect(|client| {
            client
                .select(query, Some(1), &[])?
                .first()
                .get_one::<bool>()
        })
        .map(|membership| membership.unwrap_or(false))
        .map_err(|error| {
            CatalogError::internal(
                format!("failed to check PostgreSQL role membership: {error}"),
                Path::new(""),
            )
        })
    }
}

/// Authorize the current `PostgreSQL` user for an operation.
///
/// # Errors
///
/// See [`authorize`]; membership is resolved through
/// [`PostgresRoleMembership`], so this must run inside a backend.
pub fn authorize_current_user(
    operation: Operation,
    bundle_relative_path: &Path,
) -> Result<(), CatalogError> {
    authorize(operation, &PostgresRoleMembership, bundle_relative_path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::errors::ErrorKind;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::time::{SystemTime, UNIX_EPOCH};

    struct TempTree {
        root: PathBuf,
    }

    impl TempTree {
        fn new() -> Self {
            let nonce = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("clock before epoch")
                .as_nanos();
            let root =
                std::env::temp_dir().join(format!("pgokf-security-{}-{nonce}", std::process::id()));
            fs::create_dir_all(&root).expect("create temp root");
            Self { root }
        }
    }

    impl Drop for TempTree {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    #[test]
    fn rejects_relative_paths() {
        // Arrange & Act
        let error = validate_path_syntax(Path::new("bundle/file.md"), Path::new("file.md"))
            .expect_err("relative paths must fail");

        // Assert
        assert_eq!(error.kind(), ErrorKind::InvalidParameter);
        assert_eq!(error.bundle_path(), Path::new("file.md"));
    }

    #[cfg(unix)]
    #[test]
    fn rejects_paths_containing_nul() {
        // Arrange
        use std::os::unix::ffi::OsStrExt;
        let path = Path::new(std::ffi::OsStr::from_bytes(b"/bundle/bad\0name"));

        // Act & Assert
        assert!(validate_path_syntax(path, Path::new("bad.md")).is_err());
    }

    #[test]
    fn rejects_parent_traversal_before_canonicalization() {
        // Arrange & Act
        let error = validate_path_syntax(
            Path::new("/srv/bundles/../secrets"),
            Path::new("../secrets"),
        )
        .expect_err("parent traversal must fail");

        // Assert
        assert_eq!(error.bundle_path(), Path::new("../secrets"));
    }

    #[test]
    fn canonicalizes_and_accepts_a_path_within_an_allowed_root() {
        // Arrange
        let tree = TempTree::new();
        let bundle = tree.root.join("bundle");
        fs::create_dir(&bundle).unwrap();

        // Act
        let canonical =
            canonicalize_contained_path(&bundle, std::slice::from_ref(&tree.root), Path::new("."))
                .expect("contained bundle should pass");

        // Assert
        assert_eq!(canonical, fs::canonicalize(bundle).unwrap());
    }

    #[cfg(unix)]
    #[test]
    fn rejects_symlinks_that_escape_an_allowed_root() {
        // Arrange
        use std::os::unix::fs::symlink;
        let tree = TempTree::new();
        let allowed = tree.root.join("allowed");
        let outside = tree.root.join("outside");
        fs::create_dir(&allowed).unwrap();
        fs::create_dir(&outside).unwrap();
        fs::write(outside.join("secret.md"), "secret").unwrap();
        symlink(&outside, allowed.join("escape")).unwrap();

        // Act
        let error = canonicalize_contained_path(
            &allowed.join("escape/secret.md"),
            &[allowed],
            Path::new("escape/secret.md"),
        )
        .expect_err("escaping symlink must fail");

        // Assert
        assert_eq!(error.kind(), ErrorKind::InvalidParameter);
        assert_eq!(error.bundle_path(), Path::new("escape/secret.md"));
    }

    #[test]
    fn rejects_empty_allowed_roots() {
        // Arrange
        let tree = TempTree::new();

        // Act
        let error = canonicalize_contained_path(&tree.root, &[], Path::new("."))
            .expect_err("at least one allowed root is required");

        // Assert
        assert_eq!(error.kind(), ErrorKind::InvalidParameter);
    }

    /// Fake membership oracle. Each field records *direct* membership only; the
    /// helpers below build role-grant chains (a writer is also a reader, an
    /// admin is also a writer and reader) so the tests exercise the same
    /// inheritance `pg_has_role` reports in production.
    #[derive(Default)]
    struct FakeRoles {
        admin: bool,
        writer: bool,
        reader: bool,
    }

    impl FakeRoles {
        /// A login user granted `pgokf_reader` alone.
        fn reader() -> Self {
            Self {
                reader: true,
                ..Self::default()
            }
        }

        /// A login user granted `pgokf_writer`, which inherits `pgokf_reader`.
        fn writer() -> Self {
            Self {
                writer: true,
                reader: true,
                ..Self::default()
            }
        }

        /// A login user granted `pgokf_admin`, which inherits `pgokf_writer`
        /// and thus `pgokf_reader`.
        fn admin() -> Self {
            Self {
                admin: true,
                writer: true,
                reader: true,
            }
        }
    }

    impl RoleMembership for FakeRoles {
        fn is_member_of(&self, role: &str) -> Result<bool, CatalogError> {
            Ok(match role {
                PGOKF_ADMIN_ROLE => self.admin,
                PGOKF_WRITER_ROLE => self.writer,
                PGOKF_READER_ROLE => self.reader,
                _ => false,
            })
        }
    }

    #[test]
    fn ingest_requires_writer_and_rejects_reader() {
        // Arrange
        let reader = FakeRoles::reader();

        // Act
        let error = authorize(Operation::Ingest, &reader, Path::new("bundle"))
            .expect_err("a plain reader must not ingest bundles");

        // Assert
        assert_eq!(error.kind(), ErrorKind::InsufficientPrivilege);
        assert_eq!(error.sqlstate(), "42501");
        assert!(error.message().contains(PGOKF_WRITER_ROLE));
    }

    #[test]
    fn ingest_accepts_writer() {
        // Arrange
        let writer = FakeRoles::writer();

        // Act & Assert
        assert!(authorize(Operation::Ingest, &writer, Path::new("bundle")).is_ok());
    }

    #[test]
    fn ingest_accepts_admin_by_inheritance() {
        // Arrange: an admin inherits the writer tier, so it satisfies ingestion
        // even though ingestion's minimum tier is writer.
        let admin = FakeRoles::admin();

        // Act & Assert
        assert!(authorize(Operation::Ingest, &admin, Path::new("bundle")).is_ok());
    }

    #[test]
    fn register_tier_requires_admin_and_rejects_writer() {
        // Arrange: the Register operation now guards the admin-only surface
        // (configuration writes and file-writing exports); a writer is denied.
        let writer = FakeRoles::writer();

        // Act
        let error = authorize(Operation::Register, &writer, Path::new("config"))
            .expect_err("a writer must not reach the admin-only surface");

        // Assert
        assert_eq!(error.kind(), ErrorKind::InsufficientPrivilege);
        assert_eq!(error.sqlstate(), "42501");
        assert!(error.message().contains(PGOKF_ADMIN_ROLE));
    }

    #[test]
    fn register_tier_accepts_admin() {
        // Arrange
        let admin = FakeRoles::admin();

        // Act & Assert
        assert!(authorize(Operation::Register, &admin, Path::new("config")).is_ok());
    }

    #[test]
    fn search_accepts_every_tier() {
        // Arrange: reader, writer, and admin all satisfy the reader tier.
        let tiers = [FakeRoles::reader(), FakeRoles::writer(), FakeRoles::admin()];

        // Act & Assert
        for roles in &tiers {
            assert!(authorize(Operation::Search, roles, Path::new("query")).is_ok());
        }
    }

    #[test]
    fn search_rejects_unprivileged_roles() {
        // Arrange & Act
        let error = authorize(Operation::Search, &FakeRoles::default(), Path::new("query"))
            .expect_err("unprivileged role must fail");

        // Assert
        assert_eq!(error.kind(), ErrorKind::InsufficientPrivilege);
        assert_eq!(error.sqlstate(), "42501");
    }
}
