//! Filesystem discovery of Markdown documents under a bundle root.

use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    time::SystemTime,
};

use globset::{Glob, GlobSet, GlobSetBuilder};
use walkdir::WalkDir;

use crate::{SyncConfig, SyncError, hash::hash_file};

/// Content and filesystem attributes captured for one document.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FileMetadata {
    /// Path relative to the configured root, using forward slashes.
    pub path: PathBuf,
    /// Lowercase hexadecimal BLAKE3 hash of the file contents.
    pub hash: String,
    /// File size in bytes as reported by the filesystem.
    pub size_bytes: u64,
    /// Last modification time, when the filesystem provides one.
    pub modified_at: Option<SystemTime>,
}

/// A point-in-time view of all discovered files, indexed by normalized relative path.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Snapshot {
    files: BTreeMap<PathBuf, FileMetadata>,
}

impl Snapshot {
    pub(crate) fn new(files: BTreeMap<PathBuf, FileMetadata>) -> Self {
        Self { files }
    }

    /// All discovered files in deterministic relative-path order.
    #[must_use]
    pub fn files(&self) -> &BTreeMap<PathBuf, FileMetadata> {
        &self.files
    }

    /// Look up one file by its normalized relative path.
    #[must_use]
    pub fn get(&self, path: impl AsRef<Path>) -> Option<&FileMetadata> {
        self.files.get(path.as_ref())
    }

    /// The number of discovered files.
    #[must_use]
    pub fn len(&self) -> usize {
        self.files.len()
    }

    /// Whether the snapshot contains no files.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.files.is_empty()
    }
}

/// Discover Markdown files under `config.root` and calculate their BLAKE3 hashes.
///
/// Symbolic links are never followed while walking. A symlink entry whose resolved
/// target stays inside the bundle root is skipped, because its target is (or would
/// be, if it matched the globs) discovered at its canonical path; a symlink whose
/// target resolves outside the root is rejected so a bundle can never pull content
/// from elsewhere on the filesystem. Symlinks matching an exclude pattern are
/// outside the sync contract and are skipped without being resolved.
///
/// When [`SyncConfig::max_file_bytes`] is set, each candidate's size is checked via
/// filesystem metadata before its contents are read. When [`SyncConfig::max_files`]
/// is set, the scan aborts as soon as the limit is breached.
///
/// # Errors
///
/// Returns [`SyncError`] if a glob pattern is invalid, the walk fails, a file cannot
/// be inspected or read, a symlink escapes the root, or a configured resource limit
/// is exceeded.
pub fn discover(config: &SyncConfig) -> Result<Snapshot, SyncError> {
    let includes = build_glob_set(&config.include, "include")?;
    let excludes = build_glob_set(&config.exclude, "exclude")?;
    let mut files = BTreeMap::new();

    for entry in WalkDir::new(&config.root).follow_links(false) {
        let entry = entry.map_err(|source| SyncError::Walk {
            path: config.root.clone(),
            source,
        })?;
        let absolute = entry.path();
        let relative = absolute
            .strip_prefix(&config.root)
            .map_err(|_| SyncError::OutsideRoot {
                path: absolute.to_path_buf(),
                root: config.root.clone(),
            })?;
        let path = normalized_relative_path(relative);

        if entry.path_is_symlink() {
            if !excludes.is_match(&path) {
                ensure_symlink_containment(absolute, &config.root)?;
            }
            // An in-root link contributes nothing new: its target is discovered
            // at the target's own canonical path.
            continue;
        }
        if !entry.file_type().is_file() {
            continue;
        }

        // Guarding on the extension makes the Markdown-only contract independent of
        // whether a caller supplies a broad include such as `docs/**`.
        if path.extension().and_then(|extension| extension.to_str()) != Some("md")
            || (!config.include.is_empty() && !includes.is_match(&path))
            || excludes.is_match(&path)
        {
            continue;
        }

        if let Some(limit) = config.max_files
            && files.len() >= limit
        {
            return Err(SyncError::TooManyFiles {
                count: files.len() + 1,
                limit,
            });
        }
        let metadata = fs::metadata(absolute).map_err(|source| SyncError::Metadata {
            path: absolute.to_path_buf(),
            source,
        })?;
        if let Some(limit) = config.max_file_bytes
            && metadata.len() > limit
        {
            return Err(SyncError::FileTooLarge {
                path: absolute.to_path_buf(),
                size_bytes: metadata.len(),
                limit_bytes: limit,
            });
        }

        let file = FileMetadata {
            path: path.clone(),
            hash: hash_file(absolute)?,
            size_bytes: metadata.len(),
            modified_at: metadata.modified().ok(),
        };
        tracing::debug!(path = %path.display(), hash = %file.hash, "discovered OKF document");
        files.insert(path, file);
    }

    Ok(Snapshot::new(files))
}

fn build_glob_set(patterns: &[String], kind: &'static str) -> Result<GlobSet, SyncError> {
    let mut builder = GlobSetBuilder::new();
    for pattern in patterns {
        let glob = Glob::new(pattern).map_err(|source| SyncError::InvalidGlob {
            kind,
            pattern: pattern.clone(),
            source,
        })?;
        builder.add(glob);
    }
    builder.build().map_err(|source| SyncError::InvalidGlob {
        kind,
        pattern: patterns.join(", "),
        source,
    })
}

/// Reject a symlink whose fully resolved target lies outside the bundle root.
///
/// The check applies to file and directory links alike: an escaping directory link
/// would otherwise silently hide content, and an escaping file link would let
/// [`hash_file`] read content from outside the bundle.
fn ensure_symlink_containment(link: &Path, root: &Path) -> Result<(), SyncError> {
    let canonical_root = canonicalize(root)?;
    let target = canonicalize(link)?;
    if target.starts_with(&canonical_root) {
        Ok(())
    } else {
        Err(SyncError::SymlinkEscape {
            path: link.to_path_buf(),
            root: root.to_path_buf(),
        })
    }
}

fn canonicalize(path: &Path) -> Result<PathBuf, SyncError> {
    fs::canonicalize(path).map_err(|source| SyncError::Metadata {
        path: path.to_path_buf(),
        source,
    })
}

fn normalized_relative_path(path: &Path) -> PathBuf {
    PathBuf::from(path.to_string_lossy().replace('\\', "/"))
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use super::*;
    use crate::test_support::write_file;

    #[test]
    fn discovers_only_markdown_matching_the_globs() {
        let root = TempDir::new().unwrap();
        write_file(&root, "guide/keep.md", "keep");
        write_file(&root, "guide/private.md", "hidden");
        write_file(&root, "guide/not-a-document.txt", "nope");
        let config = SyncConfig::new(root.path())
            .with_include(["guide/**/*.md"])
            .with_exclude(["**/private.md"]);

        let snapshot = discover(&config).unwrap();

        assert_eq!(snapshot.len(), 1);
        let file = snapshot.get("guide/keep.md").unwrap();
        assert_eq!(file.hash, crate::hash_bytes(b"keep"));
        assert_eq!(file.size_bytes, 4);
        assert!(file.modified_at.is_some());
    }

    #[test]
    fn an_empty_include_list_selects_all_markdown_files() {
        let root = TempDir::new().unwrap();
        write_file(&root, "root.md", "root");
        write_file(&root, "nested/document.md", "nested");
        write_file(&root, "nested/ignored.txt", "not markdown");
        let config = SyncConfig::new(root.path()).with_include(Vec::<String>::new());

        let snapshot = discover(&config).unwrap();

        assert_eq!(snapshot.len(), 2);
        assert!(snapshot.get("root.md").is_some());
        assert!(snapshot.get("nested/document.md").is_some());
    }

    #[test]
    fn an_invalid_include_glob_is_an_invalid_glob_error() {
        let root = TempDir::new().unwrap();
        let config = SyncConfig::new(root.path()).with_include(["["]);

        let result = discover(&config);

        assert!(matches!(
            result,
            Err(SyncError::InvalidGlob { kind: "include", pattern, .. }) if pattern == "["
        ));
    }

    #[test]
    fn an_invalid_exclude_glob_is_an_invalid_glob_error() {
        let root = TempDir::new().unwrap();
        let config = SyncConfig::new(root.path()).with_exclude(["["]);

        let result = discover(&config);

        assert!(matches!(
            result,
            Err(SyncError::InvalidGlob { kind: "exclude", pattern, .. }) if pattern == "["
        ));
    }

    #[test]
    fn a_missing_root_is_a_walk_error() {
        let root = TempDir::new().unwrap();
        let missing = root.path().join("no-such-bundle");
        let config = SyncConfig::new(&missing);

        let result = discover(&config);

        assert!(matches!(
            result,
            Err(SyncError::Walk { path, .. }) if path == missing
        ));
    }

    #[test]
    fn a_file_larger_than_max_file_bytes_is_rejected_before_reading() {
        let root = TempDir::new().unwrap();
        write_file(&root, "huge.md", "twelve bytes");
        let config = SyncConfig::new(root.path()).with_max_file_bytes(4);

        let result = discover(&config);

        assert!(matches!(
            result,
            Err(SyncError::FileTooLarge {
                path,
                size_bytes: 12,
                limit_bytes: 4,
            }) if path == root.path().join("huge.md")
        ));
    }

    #[test]
    fn a_bundle_with_more_files_than_max_files_is_rejected() {
        let root = TempDir::new().unwrap();
        write_file(&root, "a.md", "a");
        write_file(&root, "b.md", "b");
        write_file(&root, "c.md", "c");
        let config = SyncConfig::new(root.path()).with_max_files(2);

        let result = discover(&config);

        assert!(matches!(
            result,
            Err(SyncError::TooManyFiles { count: 3, limit: 2 })
        ));
    }

    #[test]
    fn limits_equal_to_actual_usage_are_not_exceeded() {
        let root = TempDir::new().unwrap();
        write_file(&root, "a.md", "1234");
        write_file(&root, "b.md", "5678");
        let config = SyncConfig::new(root.path())
            .with_max_file_bytes(4)
            .with_max_files(2);

        let snapshot = discover(&config).unwrap();

        assert_eq!(snapshot.len(), 2);
    }
}
