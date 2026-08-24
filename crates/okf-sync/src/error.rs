use std::path::PathBuf;

use thiserror::Error;

/// Errors produced by discovery and hashing.
#[derive(Debug, Error)]
pub enum Error {
    #[error("bundle root is not a directory: {0}")]
    InvalidRoot(PathBuf),
    #[error("invalid {kind} glob `{pattern}`: {source}")]
    InvalidGlob {
        kind: &'static str,
        pattern: String,
        #[source]
        source: glob::PatternError,
    },
    #[error("failed while walking bundle: {0}")]
    Walk(#[from] walkdir::Error),
    #[error("failed to read {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("discovered path is not valid UTF-8: {0}")]
    NonUtf8Path(PathBuf),
    #[error("symlinked file escapes bundle root: {0}")]
    SymlinkEscape(PathBuf),
}

/// Sync result type.
pub type Result<T> = std::result::Result<T, Error>;
