// SPDX-License-Identifier: AGPL-3.0-only
//! Domain error types with stable `PostgreSQL` SQLSTATE mappings.
//!
//! Every catalog operation reports failures as a [`CatalogError`], which
//! may carry a bundle-relative path so SQL clients can identify the offending
//! object, and maps onto a fixed SQLSTATE per [`ErrorKind`].

use std::fmt;
use std::path::{Path, PathBuf};

/// Domain-level error categories with stable `PostgreSQL` SQLSTATE mappings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorKind {
    /// A caller-supplied parameter is invalid (`22023`).
    InvalidParameter,
    /// The current role lacks the required privilege (`42501`).
    InsufficientPrivilege,
    /// A path collides with an already-registered one (`23505`).
    DuplicatePath,
    /// An unexpected internal failure (`XX000`).
    Internal,
}

impl ErrorKind {
    /// Five-character SQLSTATE associated with this error category.
    #[must_use]
    pub const fn sqlstate(self) -> &'static str {
        match self {
            Self::InvalidParameter => "22023",
            Self::InsufficientPrivilege => "42501",
            Self::DuplicatePath => "23505",
            Self::Internal => "XX000",
        }
    }

    /// The `pgrx` error code equivalent of [`Self::sqlstate`], used when
    /// raising the error through `PostgreSQL`'s `ereport` machinery.
    #[must_use]
    pub const fn pg_sql_error_code(self) -> pgrx::PgSqlErrorCode {
        match self {
            Self::InvalidParameter => pgrx::PgSqlErrorCode::ERRCODE_INVALID_PARAMETER_VALUE,
            Self::InsufficientPrivilege => pgrx::PgSqlErrorCode::ERRCODE_INSUFFICIENT_PRIVILEGE,
            Self::DuplicatePath => pgrx::PgSqlErrorCode::ERRCODE_UNIQUE_VIOLATION,
            Self::Internal => pgrx::PgSqlErrorCode::ERRCODE_INTERNAL_ERROR,
        }
    }
}

/// Error returned by catalog operations.
///
/// `bundle_path` identifies the offending bundle-relative object for
/// file-scoped errors (IO/parse failures). It is empty for errors that carry
/// no file context — validation, configuration, and limit checks — in which
/// case no path context is appended when the error is rendered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CatalogError {
    kind: ErrorKind,
    message: String,
    bundle_path: PathBuf,
}

impl CatalogError {
    /// Build an error of an explicit [`ErrorKind`].
    #[must_use]
    pub fn new(kind: ErrorKind, message: impl Into<String>, bundle_path: impl AsRef<Path>) -> Self {
        Self {
            kind,
            message: message.into(),
            bundle_path: bundle_path.as_ref().to_path_buf(),
        }
    }

    /// Build an [`ErrorKind::InvalidParameter`] error (SQLSTATE `22023`).
    #[must_use]
    pub fn invalid_parameter(message: impl Into<String>, bundle_path: impl AsRef<Path>) -> Self {
        Self::new(ErrorKind::InvalidParameter, message, bundle_path)
    }

    /// Build an [`ErrorKind::InsufficientPrivilege`] error (SQLSTATE `42501`).
    #[must_use]
    pub fn insufficient_privilege(
        message: impl Into<String>,
        bundle_path: impl AsRef<Path>,
    ) -> Self {
        Self::new(ErrorKind::InsufficientPrivilege, message, bundle_path)
    }

    /// Build an [`ErrorKind::DuplicatePath`] error (SQLSTATE `23505`).
    #[must_use]
    pub fn duplicate_path(message: impl Into<String>, bundle_path: impl AsRef<Path>) -> Self {
        Self::new(ErrorKind::DuplicatePath, message, bundle_path)
    }

    /// Build an [`ErrorKind::Internal`] error (SQLSTATE `XX000`).
    #[must_use]
    pub fn internal(message: impl Into<String>, bundle_path: impl AsRef<Path>) -> Self {
        Self::new(ErrorKind::Internal, message, bundle_path)
    }

    /// The error category, which determines the SQLSTATE.
    #[must_use]
    pub const fn kind(&self) -> ErrorKind {
        self.kind
    }

    /// Five-character SQLSTATE this error raises as.
    #[must_use]
    pub const fn sqlstate(&self) -> &'static str {
        self.kind.sqlstate()
    }

    /// Bundle-relative path of the object the error applies to; empty for
    /// errors with no file context (validation, configuration, limit checks).
    #[must_use]
    pub fn bundle_path(&self) -> &Path {
        &self.bundle_path
    }

    /// Human-readable message without the appended path context.
    #[must_use]
    pub fn message(&self) -> &str {
        &self.message
    }

    /// Raise this error through `PostgreSQL` with its mapped SQLSTATE.
    ///
    /// This transfers control to `PostgreSQL`'s error machinery and never
    /// returns; only call it from code running inside a backend.
    pub fn raise(self) -> ! {
        pgrx::ereport!(
            pgrx::PgLogLevel::ERROR,
            self.kind.pg_sql_error_code(),
            self.to_string()
        );
        unreachable!("PostgreSQL ERROR reports do not return")
    }
}

impl fmt::Display for CatalogError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Only file-scoped errors carry a real path; append the bundle-relative
        // context solely when one is present. Errors without file context
        // (validation, configuration, limit checks) use an empty path and are
        // rendered as the bare message, with no misleading `<bundle-root>` suffix.
        if self.bundle_path.as_os_str().is_empty() {
            write!(formatter, "{}", self.message)
        } else {
            write!(
                formatter,
                "{} [bundle-relative path: {}]",
                self.message,
                self.bundle_path.display()
            )
        }
    }
}

impl std::error::Error for CatalogError {}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn maps_domain_errors_to_required_sqlstates() {
        // Arrange & Act: build one error per category.
        // Assert: each maps to its contractually fixed SQLSTATE.
        assert_eq!(
            CatalogError::invalid_parameter("bad option", "bundle.md").sqlstate(),
            "22023"
        );
        assert_eq!(
            CatalogError::insufficient_privilege("denied", "bundle.md").sqlstate(),
            "42501"
        );
        assert_eq!(
            CatalogError::duplicate_path("duplicate", "bundle.md").sqlstate(),
            "23505"
        );
    }

    #[test]
    fn file_scoped_error_appends_bundle_relative_path() {
        // Arrange
        let error = CatalogError::invalid_parameter("bad path", Path::new("concepts/runbook.md"));

        // Act
        let rendered = error.to_string();

        // Assert
        assert!(rendered.contains("concepts/runbook.md"));
        assert!(rendered.contains("bad path"));
        assert!(rendered.contains("[bundle-relative path:"));
    }

    #[test]
    fn validation_error_without_path_has_no_suffix() {
        // Arrange: a limit/validation error carries no file context.
        let error =
            CatalogError::invalid_parameter("limit_count must be between 1 and 500, got 9999", "");

        // Act
        let rendered = error.to_string();

        // Assert: bare message, no bundle-relative path suffix.
        assert_eq!(rendered, "limit_count must be between 1 and 500, got 9999");
        assert!(!rendered.contains("[bundle-relative path:"));
        assert!(!rendered.contains("<bundle-root>"));
    }
}
