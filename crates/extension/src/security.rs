// SPDX-License-Identifier: AGPL-3.0-only
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
/// are strictly nested - `Search` (reader) < `Ingest` (writer) <
/// `Register` (admin). Because the roles inherit downward (`pgokf_admin`
/// is granted `pgokf_writer`, which is granted `pgokf_reader`), a higher tier
/// satisfies any lower requirement; [`authorize`] encodes that by accepting
/// the required role *or any higher one*.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Operation {
    /// Administrative mutation - configuration writes (`set_config`,
    /// `reset_config`) and the file-writing exports (`export_parquet`,
    /// `export_sources`). Requires `pgokf_admin`.
    ///
    /// (The variant is named for the original bundle-registration gate; bundle
    /// ingestion has since moved to the lower-privilege [`Operation::Ingest`]
    /// tier, and this variant now guards only the admin-only surface.)
    Register,
    /// Bundle ingestion - register, refresh, or unregister a bundle. Requires
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
/// own inheritance resolution - an admin is authorized for an `Ingest`
/// operation even if the role-grant chain were ever misconfigured - and keeps
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
/// higher tier satisfies any lower requirement - the check accepts the
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
    authorize(operation, &PostgresRoleMembership, bundle_relative_path)?;
    // The ingestion tier stamps rows with the session's tenant, so under the
    // require_tenant policy an unscoped session is refused here, before any
    // side effect; see enforce_active_tenant for why the admin tier is not.
    if operation == Operation::Ingest {
        enforce_active_tenant()?;
    }
    Ok(())
}

/// The SQLSTATE `22023` error every bundle-addressed entry point already raises
/// for an unregistered `bundle_id`
/// (`pgokf.refresh_bundle` / `unregister_bundle` / `set_bundle_enabled` /
/// `export_parquet` / `export_sources`).
///
/// The tenant guard reuses this identical message and shape so that a bundle
/// owned by *another* tenant is indistinguishable from one that does not exist:
/// a scoped session cannot use a guessed id to confirm whether some other
/// tenant holds a bundle.
fn unknown_bundle_error(bundle_id: i64) -> CatalogError {
    CatalogError::invalid_parameter(
        format!("bundle {bundle_id} is not registered"),
        Path::new(""),
    )
}

/// The SQLSTATE `42501` error raised when the catalog's `require_tenant`
/// policy is on and the session has not set `pgokf.tenant`.
///
/// Deliberately distinct from [`unknown_bundle_error`]: an unscoped session is
/// not probing another tenant's ids, it is misconfigured, and the message says
/// how to fix it.
fn tenant_required_error() -> CatalogError {
    CatalogError::insufficient_privilege(
        "an active tenant is required: the catalog policy require_tenant is on and \
         pgokf.tenant is not set for this session (SET pgokf.tenant = '<tenant>', or \
         pin it with ALTER ROLE ... SET pgokf.tenant)",
        Path::new(""),
    )
}

/// How the tenant rules see a session: its *raw* `pgokf.tenant` and whether
/// the catalog policy requires one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct TenantContext<'a> {
    /// `current_setting('pgokf.tenant', true)`: `None` when unset, `Some("")`
    /// when explicitly cleared, otherwise the active tenant.
    pub session_tenant: Option<&'a str>,
    /// The durable `require_tenant` policy value.
    pub tenant_required: bool,
}

impl TenantContext<'_> {
    /// Whether the session is unscoped (no tenant, or an empty one).
    fn is_unscoped(self) -> bool {
        matches!(self.session_tenant, None | Some(""))
    }

    /// Whether an unscoped session is refused outright by the policy.
    fn is_denied(self) -> bool {
        self.is_unscoped() && self.tenant_required
    }
}

/// Decide whether a session may act on a bundle, given the session's tenant
/// context and the target bundle's stored `tenant_id`.
///
/// This is the write-side mirror of the read-side row-level-security predicate
/// (`(tenant unset AND NOT tenant_required()) OR tenant_id = tenant`), factored
/// out as a pure function so the decision is unit-testable without a running
/// `PostgreSQL` server:
///
/// - a session with **no** tenant set (`None`) or an **empty** tenant (`""`) is
///   cross-tenant *by design* - the backward-compatible see-all default that
///   also lets a trusted operator/superuser operate on any bundle, exactly as
///   the read policy's "unset = all" - so it is permitted **unless** the
///   catalog's `require_tenant` policy is on, in which case it is refused
///   (the read policy then shows it nothing, too);
/// - a session **with** a tenant is confined to it: the bundle must exist
///   (`bundle_tenant` is `Some`) and its tenant must equal the session's.
///
/// A missing bundle (`bundle_tenant == None`) is therefore rejected identically
/// to a cross-tenant one, which is what makes the two indistinguishable.
fn tenant_permits(context: TenantContext<'_>, bundle_tenant: Option<&str>) -> bool {
    match context.session_tenant {
        None | Some("") => !context.tenant_required,
        Some(active) => bundle_tenant == Some(active),
    }
}

/// Read the raw session tenant, the `require_tenant` policy, and the target
/// bundle's owner in a single read-only SPI round-trip.
///
/// The session tenant is read with `current_setting('pgokf.tenant', true)` (the
/// missing-ok form), which is `NULL` when unset and `''` when explicitly
/// cleared - deliberately *not* `pgokf_private.effective_tenant()`, which
/// collapses both to the literal `'default'` and would wrongly confine an unset
/// operator session to the default tenant. The bundle owner is a correlated
/// scalar subquery, so the statement always yields exactly one row and a
/// non-existent `bundle_id` reads back as a `NULL` owner rather than no row.
///
/// The read is intentionally an owner-rights read (the caller is a
/// `SECURITY DEFINER` body): it must see the bundle regardless of the row-level
/// policy, because the whole point of the guard is to confine the very callers
/// that bypass RLS.
fn read_bundle_tenant_context(
    bundle_id: i64,
) -> Result<(Option<String>, bool, Option<String>), CatalogError> {
    pgrx::Spi::connect(|client| {
        let table = client.select(
            "SELECT pg_catalog.current_setting('pgokf.tenant', true),
                    (SELECT require_tenant FROM pgokf_private.config WHERE singleton),
                    (SELECT b.tenant_id FROM pgokf.bundles b WHERE b.id = $1)",
            Some(1),
            &[bundle_id.into()],
        )?;
        let row = table.first();
        let session_tenant = row.get::<String>(1)?;
        let tenant_required = row.get::<bool>(2)?.unwrap_or(false);
        let bundle_tenant = row.get::<String>(3)?;
        Ok((session_tenant, tenant_required, bundle_tenant))
    })
    .map_err(|error: pgrx::spi::Error| {
        CatalogError::internal(
            format!("failed to resolve bundle tenant for write confinement: {error}"),
            Path::new(""),
        )
    })
}

/// Read the session's tenant context alone (no bundle): the raw
/// `pgokf.tenant` and the `require_tenant` policy, with owner rights.
fn read_tenant_context() -> Result<(Option<String>, bool), CatalogError> {
    pgrx::Spi::connect(|client| {
        let table = client.select(
            "SELECT pg_catalog.current_setting('pgokf.tenant', true),
                    (SELECT require_tenant FROM pgokf_private.config WHERE singleton)",
            Some(1),
            &[],
        )?;
        let row = table.first();
        Ok((row.get::<String>(1)?, row.get::<bool>(2)?.unwrap_or(false)))
    })
    .map_err(|error: pgrx::spi::Error| {
        CatalogError::internal(
            format!("failed to resolve the session tenant context: {error}"),
            Path::new(""),
        )
    })
}

/// Refuse an unscoped session when the catalog's `require_tenant` policy is
/// on. Applied to the ingestion tier (`register_bundle`,
/// `register_bundle_content`, and the other `pgokf_writer` entry points) by
/// [`authorize_current_user`], so a bundle can never be stamped with the
/// implicit `'default'` tenant in a catalog that has opted out of it. The
/// admin tier is exempt on purpose: `set_config` must stay callable to turn
/// the policy on and off, and the bundle-addressed admin functions are
/// confined by [`enforce_bundle_tenant`] instead.
///
/// # Errors
///
/// Returns an [`crate::errors::ErrorKind::InsufficientPrivilege`] error
/// (SQLSTATE `42501`) when a tenant is required and none is set.
pub fn enforce_active_tenant() -> Result<(), CatalogError> {
    let (session_tenant, tenant_required) = read_tenant_context()?;
    let context = TenantContext {
        session_tenant: session_tenant.as_deref(),
        tenant_required,
    };
    if context.is_denied() {
        Err(tenant_required_error())
    } else {
        Ok(())
    }
}

/// Confine a bundle-addressed mutation or export to the session's active tenant.
///
/// This closes the write-side counterpart of the read-side row-level-security
/// isolation. The `SECURITY DEFINER` functions that take an explicit
/// `bundle_id` run as the table owner and so **bypass RLS**; without this guard
/// a `pgokf_writer` / `pgokf_admin` session that has `SET pgokf.tenant = 'acme'`
/// could still act on another tenant's bundle simply by passing its id. Calling
/// this right after the `bundle_id` is known - and before any lock, filesystem,
/// or catalog side effect - makes write confinement equal to read confinement.
///
/// The rule mirrors [`tenant_permits`] exactly: when `pgokf.tenant` is unset or
/// empty the caller is cross-tenant by design (backward compatible; the trusted
/// operator/superuser path), so nothing is restricted - unless the catalog's
/// `require_tenant` policy is on, which refuses the unscoped call with SQLSTATE
/// `42501` ([`tenant_required_error`]); when a tenant is set, a
/// bundle owned by any other tenant - or one that does not exist - is rejected
/// with the same [`unknown_bundle_error`] (`22023`) the entry points already
/// raise for a bad id, so a cross-tenant id cannot be used to probe another
/// tenant's catalog.
///
/// Because this is an explicit check rather than RLS, it confines every caller,
/// including a superuser or the extension owner running inside the
/// `SECURITY DEFINER` bodies - precisely the callers RLS does not.
///
/// # Errors
///
/// Returns an [`crate::errors::ErrorKind::InvalidParameter`] error (SQLSTATE
/// `22023`) when a tenant is set and the target bundle is owned by a different
/// tenant or is unregistered, or an [`crate::errors::ErrorKind::Internal`] error
/// if the read-only tenant lookup itself fails.
pub fn enforce_bundle_tenant(bundle_id: i64) -> Result<(), CatalogError> {
    let (session_tenant, tenant_required, bundle_tenant) = read_bundle_tenant_context(bundle_id)?;
    let context = TenantContext {
        session_tenant: session_tenant.as_deref(),
        tenant_required,
    };
    if context.is_denied() {
        Err(tenant_required_error())
    } else if tenant_permits(context, bundle_tenant.as_deref()) {
        Ok(())
    } else {
        Err(unknown_bundle_error(bundle_id))
    }
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

    fn context(session_tenant: Option<&str>, tenant_required: bool) -> TenantContext<'_> {
        TenantContext {
            session_tenant,
            tenant_required,
        }
    }

    #[test]
    fn tenant_permits_allows_an_unset_session_on_any_bundle() {
        // Arrange: no pgokf.tenant set (the backward-compatible see-all default)
        // and the policy not requiring one.
        // Act & Assert: permitted for a matching, a foreign, and a missing bundle.
        assert!(tenant_permits(context(None, false), Some("acme")));
        assert!(tenant_permits(context(None, false), Some("globex")));
        assert!(tenant_permits(context(None, false), None));
    }

    #[test]
    fn tenant_permits_treats_an_empty_tenant_as_unset() {
        // Arrange: pgokf.tenant explicitly cleared to '' behaves as unset.
        // Act & Assert
        assert!(tenant_permits(context(Some(""), false), Some("globex")));
        assert!(tenant_permits(context(Some(""), false), None));
    }

    #[test]
    fn tenant_permits_refuses_an_unset_session_when_a_tenant_is_required() {
        // Arrange: the require_tenant policy is on and the session is unscoped
        // (unset or cleared).
        // Act & Assert: refused for every bundle, existing or not.
        assert!(!tenant_permits(context(None, true), Some("acme")));
        assert!(!tenant_permits(context(Some(""), true), Some("acme")));
        assert!(!tenant_permits(context(None, true), None));
    }

    #[test]
    fn tenant_permits_confines_a_scoped_session_to_its_own_tenant() {
        // Arrange: a session scoped to acme, with and without the policy on
        // (the policy only concerns unscoped sessions).
        // Act & Assert: only acme's own bundle is permitted.
        for required in [false, true] {
            assert!(tenant_permits(
                context(Some("acme"), required),
                Some("acme")
            ));
            assert!(!tenant_permits(
                context(Some("acme"), required),
                Some("globex")
            ));
        }
    }

    #[test]
    fn tenant_permits_rejects_a_missing_bundle_for_a_scoped_session() {
        // Arrange: a scoped session naming a bundle that does not exist - must be
        // rejected identically to a cross-tenant one so the two are
        // indistinguishable.
        // Act & Assert
        assert!(!tenant_permits(context(Some("acme"), false), None));
    }

    #[test]
    fn tenant_context_is_denied_only_when_unscoped_and_required() {
        // Arrange & Act & Assert: the four combinations.
        assert!(context(None, true).is_denied());
        assert!(context(Some(""), true).is_denied());
        assert!(!context(None, false).is_denied());
        assert!(!context(Some("acme"), true).is_denied());
    }

    #[test]
    fn tenant_required_error_is_a_42501_with_the_fix_in_the_message() {
        // Arrange & Act
        let error = tenant_required_error();

        // Assert
        assert_eq!(error.sqlstate(), "42501");
        assert!(error.to_string().contains("require_tenant"));
        assert!(error.to_string().contains("SET pgokf.tenant"));
    }

    #[test]
    fn unknown_bundle_error_is_the_shared_22023_shape() {
        // Arrange & Act
        let error = unknown_bundle_error(7);

        // Assert: the same SQLSTATE, kind, and message shape the bundle-addressed
        // entry points already raise for a bad id (so a cross-tenant bundle looks
        // unknown, not "forbidden").
        assert_eq!(error.kind(), ErrorKind::InvalidParameter);
        assert_eq!(error.sqlstate(), "22023");
        assert!(error.message().contains('7'));
        assert!(error.message().contains("not registered"));
    }
}
