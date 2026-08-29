// SPDX-License-Identifier: AGPL-3.0-only
//! BLAKE3 content hashing helpers.
//!
//! Both helpers produce the same lowercase hexadecimal digest for the same
//! content, so a document hashed from memory (for example, content already
//! loaded by the extension) compares directly against one hashed from disk.

use std::{fs::File, path::Path};

use crate::SyncError;

/// Hash in-memory content, returning the lowercase hexadecimal BLAKE3 digest.
#[must_use]
pub fn hash_bytes(bytes: &[u8]) -> String {
    blake3::hash(bytes).to_hex().to_string()
}

/// Stream a file through BLAKE3 without loading it entirely into memory,
/// returning the lowercase hexadecimal digest.
///
/// # Errors
///
/// Returns [`SyncError::Read`] when the file cannot be opened or read.
pub fn hash_file(path: &Path) -> Result<String, SyncError> {
    let read_error = |source| SyncError::Read {
        path: path.to_path_buf(),
        source,
    };
    let file = File::open(path).map_err(read_error)?;
    let mut hasher = blake3::Hasher::new();
    hasher.update_reader(file).map_err(read_error)?;
    Ok(hasher.finalize().to_hex().to_string())
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::TempDir;

    use super::*;

    /// The well-known BLAKE3 digest of the empty input.
    const EMPTY_BLAKE3: &str = "af1349b9f5f9a1a6a0404dea36dcc9499bcb25c9adc112b7cc9a93cae41f3262";

    #[test]
    fn hash_bytes_returns_the_lowercase_hex_blake3_digest() {
        let digest = hash_bytes(b"");

        assert_eq!(digest, EMPTY_BLAKE3);
    }

    #[test]
    fn hash_file_matches_hash_bytes_for_the_same_content() {
        let root = TempDir::new().unwrap();
        let path = root.path().join("concept.md");
        fs::write(&path, b"# Concept\n").unwrap();

        let from_disk = hash_file(&path).unwrap();

        assert_eq!(from_disk, hash_bytes(b"# Concept\n"));
        assert_eq!(from_disk.len(), 64);
    }

    #[test]
    fn hash_file_on_a_missing_path_is_a_read_error() {
        let root = TempDir::new().unwrap();
        let missing = root.path().join("does-not-exist.md");

        let result = hash_file(&missing);

        assert!(matches!(
            result,
            Err(SyncError::Read { path, .. }) if path == missing
        ));
    }
}
