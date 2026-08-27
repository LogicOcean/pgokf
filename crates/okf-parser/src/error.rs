use thiserror::Error;

/// High-level classification of a parse failure.
///
/// The sync layer aggregates per-file diagnostics by category, so every
/// [`Error`] variant maps to exactly one category via [`Error::category`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ErrorCategory {
    /// A configured resource limit was exceeded.
    Limit,
    /// The source bytes are not valid UTF-8.
    Encoding,
    /// The YAML frontmatter is missing, unterminated, or invalid.
    Frontmatter,
    /// Frontmatter values cannot be represented as JSON metadata.
    Metadata,
    /// The bundle-relative path is empty, absolute, traversing, non-UTF-8,
    /// or not a Markdown file.
    Path,
    /// The path names a reserved OKF file (`index.md` / `log.md`).
    Reserved,
}

/// Errors returned while parsing and normalizing a concept.
///
/// Every variant that concerns a file carries the file path so callers can
/// attach diagnostics to the offending file without extra bookkeeping; use
/// [`Error::path`] and [`Error::category`] for uniform access.
#[derive(Debug, Error)]
pub enum Error {
    /// The concept file exceeds the configured file-size limit.
    #[error("{path}: concept file is {actual} bytes, exceeding the {limit}-byte limit")]
    FileTooLarge {
        /// Normalized bundle-relative path of the offending file.
        path: String,
        /// Observed size in bytes.
        actual: usize,
        /// Configured maximum in bytes.
        limit: usize,
    },
    /// The YAML frontmatter block exceeds the configured size limit.
    #[error("{path}: frontmatter is {actual} bytes, exceeding the {limit}-byte limit")]
    FrontmatterTooLarge {
        /// Normalized bundle-relative path of the offending file.
        path: String,
        /// Observed size in bytes.
        actual: usize,
        /// Configured maximum in bytes.
        limit: usize,
    },
    /// The file does not begin with a `---` frontmatter delimiter.
    #[error("{path}: Markdown file must begin with a YAML frontmatter delimiter (`---`)")]
    MissingFrontmatter {
        /// Normalized bundle-relative path of the offending file.
        path: String,
    },
    /// The frontmatter block has no closing `---` delimiter.
    #[error("{path}: YAML frontmatter has no closing `---` delimiter")]
    UnterminatedFrontmatter {
        /// Normalized bundle-relative path of the offending file.
        path: String,
    },
    /// The frontmatter block is not valid YAML or misses required keys.
    #[error("{path}: invalid YAML frontmatter: {source}")]
    InvalidFrontmatter {
        /// Normalized bundle-relative path of the offending file.
        path: String,
        /// Underlying YAML deserialization failure.
        #[source]
        source: serde_yaml::Error,
    },
    /// A frontmatter value cannot be represented as JSON metadata.
    #[error("{path}: frontmatter metadata cannot be represented as JSON: {source}")]
    InvalidMetadata {
        /// Normalized bundle-relative path of the offending file.
        path: String,
        /// Underlying JSON conversion failure.
        #[source]
        source: serde_json::Error,
    },
    /// The concept source bytes are not valid UTF-8.
    #[error("{path}: concept source is not valid UTF-8: {source}")]
    InvalidUtf8 {
        /// Normalized bundle-relative path of the offending file.
        path: String,
        /// Underlying UTF-8 decoding failure.
        #[source]
        source: std::str::Utf8Error,
    },
    /// The supplied concept path is empty.
    #[error("concept path is empty")]
    EmptyPath,
    /// The supplied concept path is absolute instead of bundle-relative.
    #[error("concept path must be relative: {path}")]
    AbsolutePath {
        /// The offending path as supplied.
        path: String,
    },
    /// The supplied concept path traverses above the bundle root.
    #[error("concept path contains traversal: {path}")]
    PathTraversal {
        /// The offending path as supplied.
        path: String,
    },
    /// The supplied concept path does not end in `.md`.
    #[error("concept path must have a .md extension: {path}")]
    UnsupportedExtension {
        /// The offending path as supplied.
        path: String,
    },
    /// The supplied concept path contains non-UTF-8 data.
    #[error("concept path contains non-UTF-8 data: {path}")]
    NonUtf8Path {
        /// Lossy UTF-8 rendering of the offending path, for diagnostics.
        path: String,
    },
    /// The path names a reserved OKF file, which is not an ordinary concept.
    #[error("{path}: reserved OKF file (`index.md`/`log.md`) is not an ordinary concept")]
    ReservedPath {
        /// Normalized bundle-relative path of the reserved file.
        path: String,
    },
}

impl Error {
    /// The category this error belongs to, for per-file diagnostic rollups.
    #[must_use]
    pub fn category(&self) -> ErrorCategory {
        match self {
            Self::FileTooLarge { .. } | Self::FrontmatterTooLarge { .. } => ErrorCategory::Limit,
            Self::InvalidUtf8 { .. } => ErrorCategory::Encoding,
            Self::MissingFrontmatter { .. }
            | Self::UnterminatedFrontmatter { .. }
            | Self::InvalidFrontmatter { .. } => ErrorCategory::Frontmatter,
            Self::InvalidMetadata { .. } => ErrorCategory::Metadata,
            Self::EmptyPath
            | Self::AbsolutePath { .. }
            | Self::PathTraversal { .. }
            | Self::UnsupportedExtension { .. }
            | Self::NonUtf8Path { .. } => ErrorCategory::Path,
            Self::ReservedPath { .. } => ErrorCategory::Reserved,
        }
    }

    /// The path of the file this error concerns.
    ///
    /// Content errors carry the normalized bundle-relative path; path errors
    /// carry the offending path as supplied. [`Error::EmptyPath`] returns the
    /// empty string, which is the offending path itself.
    #[must_use]
    pub fn path(&self) -> &str {
        match self {
            Self::EmptyPath => "",
            Self::FileTooLarge { path, .. }
            | Self::FrontmatterTooLarge { path, .. }
            | Self::MissingFrontmatter { path }
            | Self::UnterminatedFrontmatter { path }
            | Self::InvalidFrontmatter { path, .. }
            | Self::InvalidMetadata { path, .. }
            | Self::InvalidUtf8 { path, .. }
            | Self::AbsolutePath { path }
            | Self::PathTraversal { path }
            | Self::UnsupportedExtension { path }
            | Self::NonUtf8Path { path }
            | Self::ReservedPath { path } => path,
        }
    }
}

/// Parser result type.
pub type Result<T> = std::result::Result<T, Error>;
