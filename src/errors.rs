use std::fmt;
use std::path::{Path, PathBuf};

/// Domain-level error categories with stable PostgreSQL SQLSTATE mappings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorKind {
    InvalidParameter,
    InsufficientPrivilege,
    DuplicatePath,
    Internal,
}

impl ErrorKind {
    /// Five-character SQLSTATE associated with this error category.
    pub const fn sqlstate(self) -> &'static str {
        match self {
            Self::InvalidParameter => "22023",
            Self::InsufficientPrivilege => "42501",
            Self::DuplicatePath => "23505",
            Self::Internal => "XX000",
        }
    }

    #[cfg(not(test))]
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
/// `bundle_path` is always present so SQL errors can identify the offending
/// bundle-relative object. An empty path means the error applies to the bundle
/// root and is rendered explicitly as `<bundle-root>`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CatalogError {
    kind: ErrorKind,
    message: String,
    bundle_path: PathBuf,
}

impl CatalogError {
    pub fn new(kind: ErrorKind, message: impl Into<String>, bundle_path: impl AsRef<Path>) -> Self {
        Self {
            kind,
            message: message.into(),
            bundle_path: bundle_path.as_ref().to_path_buf(),
        }
    }

    pub fn invalid_parameter(message: impl Into<String>, bundle_path: impl AsRef<Path>) -> Self {
        Self::new(ErrorKind::InvalidParameter, message, bundle_path)
    }

    pub fn insufficient_privilege(
        message: impl Into<String>,
        bundle_path: impl AsRef<Path>,
    ) -> Self {
        Self::new(ErrorKind::InsufficientPrivilege, message, bundle_path)
    }

    pub fn duplicate_path(message: impl Into<String>, bundle_path: impl AsRef<Path>) -> Self {
        Self::new(ErrorKind::DuplicatePath, message, bundle_path)
    }

    pub fn internal(message: impl Into<String>, bundle_path: impl AsRef<Path>) -> Self {
        Self::new(ErrorKind::Internal, message, bundle_path)
    }

    pub const fn kind(&self) -> ErrorKind {
        self.kind
    }

    pub const fn sqlstate(&self) -> &'static str {
        self.kind.sqlstate()
    }

    pub fn bundle_path(&self) -> &Path {
        &self.bundle_path
    }

    pub fn message(&self) -> &str {
        &self.message
    }

    /// Raise this error through PostgreSQL with its mapped SQLSTATE.
    #[cfg(not(test))]
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
        let bundle_path = if self.bundle_path.as_os_str().is_empty() {
            "<bundle-root>".into()
        } else {
            self.bundle_path.display().to_string()
        };
        write!(
            formatter,
            "{} [bundle-relative path: {bundle_path}]",
            self.message
        )
    }
}

impl std::error::Error for CatalogError {}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn maps_domain_errors_to_required_sqlstates() {
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
    fn display_always_contains_bundle_relative_path() {
        let error = CatalogError::invalid_parameter("bad path", Path::new("concepts/runbook.md"));
        let rendered = error.to_string();
        assert!(rendered.contains("concepts/runbook.md"));
        assert!(rendered.contains("bad path"));
    }

    #[test]
    fn empty_bundle_path_is_rendered_explicitly() {
        let error = CatalogError::internal("unexpected", Path::new(""));
        assert!(error.to_string().contains("<bundle-root>"));
    }
}
