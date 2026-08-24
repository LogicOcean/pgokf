use thiserror::Error;

/// Errors returned while parsing and normalizing a concept.
#[derive(Debug, Error)]
pub enum Error {
    #[error("concept file is {actual} bytes, exceeding the {limit}-byte limit")]
    FileTooLarge { actual: usize, limit: usize },
    #[error("frontmatter is {actual} bytes, exceeding the {limit}-byte limit")]
    FrontmatterTooLarge { actual: usize, limit: usize },
    #[error("Markdown file must begin with a YAML frontmatter delimiter (`---`)")]
    MissingFrontmatter,
    #[error("YAML frontmatter has no closing `---` delimiter")]
    UnterminatedFrontmatter,
    #[error("invalid YAML frontmatter: {0}")]
    InvalidFrontmatter(#[from] serde_yaml::Error),
    #[error("frontmatter metadata cannot be represented as JSON: {0}")]
    InvalidMetadata(#[source] serde_json::Error),
    #[error("concept source is not valid UTF-8: {0}")]
    InvalidUtf8(#[from] std::str::Utf8Error),
    #[error("concept path is empty")]
    EmptyPath,
    #[error("concept path must be relative: {0}")]
    AbsolutePath(String),
    #[error("concept path contains traversal: {0}")]
    PathTraversal(String),
    #[error("concept path must have a .md extension: {0}")]
    UnsupportedExtension(String),
    #[error("concept path contains non-UTF-8 data")]
    NonUtf8Path,
}

/// Parser result type.
pub type Result<T> = std::result::Result<T, Error>;
