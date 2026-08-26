use std::path::{Component, Path, PathBuf};

#[cfg(test)]
#[path = "errors.rs"]
mod errors;

#[cfg(not(test))]
use crate::errors::CatalogError;
#[cfg(test)]
use errors::{CatalogError, ErrorKind};

pub const PGOKF_ADMIN_ROLE: &str = "pgokf_admin";
pub const PGOKF_READER_ROLE: &str = "pgokf_reader";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Operation {
    Register,
    Refresh,
    Search,
}

/// Abstract role lookup so authorization policy can be tested without a
/// running PostgreSQL server.
pub trait RoleMembership {
    fn is_member_of(&self, role: &str) -> Result<bool, CatalogError>;
}

/// Reject unsafe path syntax before any filesystem access.
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

/// Canonicalize and validate configured roots.
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

/// Enforce operation-specific role policy.
pub fn authorize<R: RoleMembership>(
    operation: Operation,
    roles: &R,
    bundle_relative_path: &Path,
) -> Result<(), CatalogError> {
    let membership = |role| {
        roles
            .is_member_of(role)
            .map_err(|error| CatalogError::new(error.kind(), error.message(), bundle_relative_path))
    };
    let is_admin = membership(PGOKF_ADMIN_ROLE)?;
    let authorized = match operation {
        Operation::Register | Operation::Refresh => is_admin,
        Operation::Search => is_admin || membership(PGOKF_READER_ROLE)?,
    };

    if authorized {
        Ok(())
    } else {
        let required = match operation {
            Operation::Register | Operation::Refresh => PGOKF_ADMIN_ROLE,
            Operation::Search => "pgokf_reader or pgokf_admin",
        };
        Err(CatalogError::insufficient_privilege(
            format!("operation requires membership in {required}"),
            bundle_relative_path,
        ))
    }
}

#[cfg(not(test))]
pub struct PostgresRoleMembership;

#[cfg(not(test))]
impl RoleMembership for PostgresRoleMembership {
    fn is_member_of(&self, role: &str) -> Result<bool, CatalogError> {
        let query = match role {
            PGOKF_ADMIN_ROLE => {
                "SELECT pg_catalog.pg_has_role(current_user, 'pgokf_admin', 'MEMBER')"
            }
            PGOKF_READER_ROLE => {
                "SELECT pg_catalog.pg_has_role(current_user, 'pgokf_reader', 'MEMBER')"
            }
            _ => {
                return Err(CatalogError::internal(
                    format!("unknown authorization role: {role}"),
                    Path::new(""),
                ));
            }
        };

        pgrx::Spi::get_one::<bool>(query)
            .map(|membership| membership.unwrap_or(false))
            .map_err(|error| {
                CatalogError::internal(
                    format!("failed to check PostgreSQL role membership: {error}"),
                    Path::new(""),
                )
            })
    }
}

/// Authorize the current PostgreSQL user for an operation.
#[cfg(not(test))]
pub fn authorize_current_user(
    operation: Operation,
    bundle_relative_path: &Path,
) -> Result<(), CatalogError> {
    authorize(operation, &PostgresRoleMembership, bundle_relative_path)
}

#[cfg(test)]
mod tests {
    use super::*;
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
        let error = validate_path_syntax(Path::new("bundle/file.md"), Path::new("file.md"))
            .expect_err("relative paths must fail");
        assert_eq!(error.kind(), ErrorKind::InvalidParameter);
        assert_eq!(error.bundle_path(), Path::new("file.md"));
    }

    #[cfg(unix)]
    #[test]
    fn rejects_paths_containing_nul() {
        use std::os::unix::ffi::OsStrExt;
        let path = Path::new(std::ffi::OsStr::from_bytes(b"/bundle/bad\0name"));
        assert!(validate_path_syntax(path, Path::new("bad.md")).is_err());
    }

    #[test]
    fn rejects_parent_traversal_before_canonicalization() {
        let error = validate_path_syntax(
            Path::new("/srv/bundles/../secrets"),
            Path::new("../secrets"),
        )
        .expect_err("parent traversal must fail");
        assert_eq!(error.bundle_path(), Path::new("../secrets"));
    }

    #[test]
    fn canonicalizes_and_accepts_a_path_within_an_allowed_root() {
        let tree = TempTree::new();
        let bundle = tree.root.join("bundle");
        fs::create_dir(&bundle).unwrap();
        let canonical = canonicalize_contained_path(&bundle, &[tree.root.clone()], Path::new("."))
            .expect("contained bundle should pass");
        assert_eq!(canonical, fs::canonicalize(bundle).unwrap());
    }

    #[cfg(unix)]
    #[test]
    fn rejects_symlinks_that_escape_an_allowed_root() {
        use std::os::unix::fs::symlink;
        let tree = TempTree::new();
        let allowed = tree.root.join("allowed");
        let outside = tree.root.join("outside");
        fs::create_dir(&allowed).unwrap();
        fs::create_dir(&outside).unwrap();
        fs::write(outside.join("secret.md"), "secret").unwrap();
        symlink(&outside, allowed.join("escape")).unwrap();

        let error = canonicalize_contained_path(
            &allowed.join("escape/secret.md"),
            &[allowed],
            Path::new("escape/secret.md"),
        )
        .expect_err("escaping symlink must fail");
        assert_eq!(error.kind(), ErrorKind::InvalidParameter);
        assert_eq!(error.bundle_path(), Path::new("escape/secret.md"));
    }

    #[test]
    fn rejects_empty_allowed_roots() {
        let tree = TempTree::new();
        let error = canonicalize_contained_path(&tree.root, &[], Path::new("."))
            .expect_err("at least one allowed root is required");
        assert_eq!(error.kind(), ErrorKind::InvalidParameter);
    }

    #[derive(Default)]
    struct FakeRoles {
        admin: bool,
        reader: bool,
    }

    impl RoleMembership for FakeRoles {
        fn is_member_of(&self, role: &str) -> Result<bool, CatalogError> {
            Ok(match role {
                PGOKF_ADMIN_ROLE => self.admin,
                PGOKF_READER_ROLE => self.reader,
                _ => false,
            })
        }
    }

    #[test]
    fn register_and_refresh_require_admin() {
        let reader = FakeRoles {
            reader: true,
            admin: false,
        };
        for operation in [Operation::Register, Operation::Refresh] {
            let error = authorize(operation, &reader, Path::new("bundle"))
                .expect_err("reader must not mutate bundles");
            assert_eq!(error.kind(), ErrorKind::InsufficientPrivilege);
        }
    }

    #[test]
    fn search_accepts_reader_or_admin() {
        let reader = FakeRoles {
            reader: true,
            admin: false,
        };
        let admin = FakeRoles {
            reader: false,
            admin: true,
        };
        assert!(authorize(Operation::Search, &reader, Path::new("query")).is_ok());
        assert!(authorize(Operation::Search, &admin, Path::new("query")).is_ok());
    }

    #[test]
    fn search_rejects_unprivileged_roles() {
        let error = authorize(Operation::Search, &FakeRoles::default(), Path::new("query"))
            .expect_err("unprivileged role must fail");
        assert_eq!(error.kind(), ErrorKind::InsufficientPrivilege);
        assert_eq!(error.sqlstate(), "42501");
    }
}
