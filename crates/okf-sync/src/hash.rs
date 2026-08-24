use std::fmt;
use std::fs::File;
use std::io::Read;
use std::path::Path;

use crate::{Error, Result};

/// A lowercase hexadecimal BLAKE3 digest.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct FileHash(String);

impl FileHash {
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for FileHash {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

/// Hash in-memory content.
#[must_use]
pub fn hash_bytes(bytes: &[u8]) -> FileHash {
    FileHash(blake3::hash(bytes).to_hex().to_string())
}

/// Stream a file through BLAKE3 without loading it all into memory.
///
/// # Errors
/// Returns an error when the file cannot be opened or read.
pub fn hash_file(path: &Path) -> Result<FileHash> {
    let mut file = File::open(path).map_err(|source| Error::Io {
        path: path.to_path_buf(),
        source,
    })?;
    let mut hasher = blake3::Hasher::new();
    let mut buffer = vec![0_u8; 64 * 1024];
    loop {
        let count = file.read(&mut buffer).map_err(|source| Error::Io {
            path: path.to_path_buf(),
            source,
        })?;
        if count == 0 {
            break;
        }
        hasher.update(&buffer[..count]);
    }
    Ok(FileHash(hasher.finalize().to_hex().to_string()))
}
