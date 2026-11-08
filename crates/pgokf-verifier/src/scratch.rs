//! Uniquely owned scratch beneath the platform-selected temporary root.
use std::path::Component;

/// Keep ownership in `TempDir`: callers must retain it until their processes
/// stop, explicitly close it when reporting cleanup, and retain it if stopping
/// cannot be proven. Never fall back to a different root on failure.
pub(crate) fn directory(prefix: &str) -> Result<tempfile::TempDir, String> {
    if prefix.is_empty()
        || !prefix
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-')
    {
        return Err("scratch prefix must be a single portable name".into());
    }
    // Remove trailing separators and current-directory components before lstat:
    // otherwise a symlink leaf followed by a separator would be followed.
    let root: std::path::PathBuf = std::env::temp_dir().components().collect();
    if !root.is_absolute() || root.components().any(|c| matches!(c, Component::ParentDir)) {
        return Err("temporary root must be absolute without traversal".into());
    }
    // Platform ancestors may be aliases (for example the system temporary
    // directory on macOS), but a caller-selected leaf must not redirect writes.
    let metadata = std::fs::symlink_metadata(&root).map_err(|e| e.to_string())?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err("temporary root must be a directory, not a symlink".into());
    }
    let root = root.canonicalize().map_err(|e| e.to_string())?;
    tempfile::Builder::new()
        .prefix(prefix)
        .tempdir_in(root)
        .map_err(|e| e.to_string())
}
