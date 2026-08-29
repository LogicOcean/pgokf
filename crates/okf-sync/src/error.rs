// SPDX-License-Identifier: AGPL-3.0-only
//! Error types produced by discovery, hashing, and planning.

use std::path::PathBuf;

use thiserror::Error;

/// Errors produced while discovering or hashing bundle content.
///
/// Every variant carries the paths and limits needed to act on the failure, so
/// callers (including the `PostgreSQL` extension) can surface the message verbatim.
#[derive(Debug, Error)]
pub enum SyncError {
    /// An include or exclude pattern could not be compiled.
    #[error("invalid {kind} glob `{pattern}`: {source}")]
    InvalidGlob {
        /// Which pattern list the glob came from: `"include"` or `"exclude"`.
        kind: &'static str,
        /// The offending pattern (or joined patterns when the set fails to build).
        pattern: String,
        /// The underlying compile error.
        #[source]
        source: globset::Error,
    },
    /// Filesystem metadata for an entry could not be read.
    #[error("failed to inspect `{path}`: {source}")]
    Metadata {
        /// The entry being inspected.
        path: PathBuf,
        /// The underlying I/O error.
        #[source]
        source: std::io::Error,
    },
    /// A file's contents could not be opened or read.
    #[error("failed to read `{path}`: {source}")]
    Read {
        /// The file being read.
        path: PathBuf,
        /// The underlying I/O error.
        #[source]
        source: std::io::Error,
    },
    /// The directory walk itself failed.
    #[error("failed to walk `{path}`: {source}")]
    Walk {
        /// The root being walked.
        path: PathBuf,
        /// The underlying walk error.
        #[source]
        source: walkdir::Error,
    },
    /// A discovered entry does not live under the configured root.
    #[error("`{path}` is outside sync root `{root}`")]
    OutsideRoot {
        /// The escaping entry.
        path: PathBuf,
        /// The configured sync root.
        root: PathBuf,
    },
    /// A symlink inside the bundle resolves to a target outside the bundle root.
    #[error(
        "symlink `{path}` resolves outside bundle root `{root}`; move the target into the \
         bundle, exclude the link, or remove it"
    )]
    SymlinkEscape {
        /// The symlink entry inside the bundle.
        path: PathBuf,
        /// The bundle root the target escapes.
        root: PathBuf,
    },
    /// A discovered file exceeds the configured per-file size limit.
    #[error(
        "file `{path}` is {size_bytes} bytes, exceeding the configured maximum of \
         {limit_bytes} bytes; raise the limit or exclude the file"
    )]
    FileTooLarge {
        /// The oversized file.
        path: PathBuf,
        /// The file's actual size in bytes.
        size_bytes: u64,
        /// The configured `max_file_bytes` limit.
        limit_bytes: u64,
    },
    /// A scan discovered more files than the configured limit allows.
    #[error(
        "bundle contains at least {count} matching files, exceeding the configured maximum \
         of {limit}; raise the limit or narrow the include patterns"
    )]
    TooManyFiles {
        /// The number of matching files found when the limit was breached.
        count: usize,
        /// The configured `max_files` limit.
        limit: usize,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn outside_root_error_reports_both_paths() {
        let error = SyncError::OutsideRoot {
            path: PathBuf::from("/elsewhere/doc.md"),
            root: PathBuf::from("/bundles/handbook"),
        };

        let message = error.to_string();

        assert_eq!(
            message,
            "`/elsewhere/doc.md` is outside sync root `/bundles/handbook`"
        );
    }

    #[test]
    fn resource_limit_errors_state_the_observed_value_and_the_limit() {
        let too_large = SyncError::FileTooLarge {
            path: PathBuf::from("/bundles/handbook/huge.md"),
            size_bytes: 2_048,
            limit_bytes: 1_024,
        };
        let too_many = SyncError::TooManyFiles {
            count: 11,
            limit: 10,
        };

        let large_message = too_large.to_string();
        let many_message = too_many.to_string();

        assert!(large_message.contains("2048 bytes"));
        assert!(large_message.contains("maximum of 1024 bytes"));
        assert!(many_message.contains("at least 11 matching files"));
        assert!(many_message.contains("maximum of 10"));
    }
}
