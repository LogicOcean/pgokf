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
    /// Path relative to the configured root, using forward-slash separators.
    /// File-name bytes are preserved verbatim (a POSIX name may itself
    /// contain `\`), so joining this path onto the root always resolves
    /// to the discovered file.
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
/// Symbolic links are never followed while walking. Containment is enforced only on
/// links that are themselves candidate documents — a Markdown name selected by the
/// include globs and not excluded — because only those could contribute content. A
/// candidate link whose target stays inside the bundle root is skipped, since that
/// target is discovered at its own canonical path; a candidate link whose target
/// resolves outside the root is rejected so a bundle can never pull content from
/// elsewhere on the filesystem. Any other link — the wrong extension, unmatched by
/// the globs, excluded, or dangling — is ignored without being resolved, so a stray
/// or broken link anywhere in the tree can never abort discovery.
///
/// When [`SyncConfig::max_file_bytes`] is set, each candidate's size is checked via
/// filesystem metadata before its contents are read. When [`SyncConfig::max_files`]
/// is set, the scan aborts as soon as the limit is breached.
///
/// # Errors
///
/// Returns [`SyncError`] if a glob pattern is invalid, the walk fails, a file cannot
/// be inspected or read, a candidate symlink escapes the root, or a configured
/// resource limit is exceeded.
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
            // Containment is a contract on candidate documents only. A link that is
            // not selectable as an OKF document — the wrong extension, unmatched by
            // the include globs, or excluded — is ignored without being resolved, so
            // a stray link elsewhere in the tree can never abort discovery.
            if is_candidate(&path, config, &includes, &excludes) {
                match ensure_symlink_containment(absolute, &config.root) {
                    Ok(()) => {}
                    Err(SyncError::Metadata { source, .. })
                        if source.kind() == std::io::ErrorKind::NotFound =>
                    {
                        // The link dangles: its target resolves to nothing, so there
                        // is no in-bundle content to index and nothing to escape.
                    }
                    Err(other) => return Err(other),
                }
            }
            // A resolvable in-root link contributes nothing new: its target is
            // discovered at the target's own canonical path.
            continue;
        }
        if !entry.file_type().is_file() {
            continue;
        }

        if !is_candidate(&path, config, &includes, &excludes) {
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

/// Whether a bundle-relative path is a candidate OKF document: a Markdown file
/// selected by the include globs and not excluded.
///
/// Membership depends only on the path, so it can be decided for a symlink without
/// resolving its target. Guarding on the extension makes the Markdown-only contract
/// independent of whether a caller supplies a broad include such as `docs/**`.
fn is_candidate(path: &Path, config: &SyncConfig, includes: &GlobSet, excludes: &GlobSet) -> bool {
    path.extension().and_then(|extension| extension.to_str()) == Some("md")
        && (config.include.is_empty() || includes.is_match(path))
        && !excludes.is_match(path)
}

/// Reject a candidate symlink whose fully resolved target lies outside the bundle
/// root.
///
/// Only candidate Markdown links reach this check: an escaping file link would
/// otherwise let [`hash_file`] read content from outside the bundle. A target that
/// cannot be resolved surfaces as a [`SyncError::Metadata`] error carrying the
/// underlying I/O error, which the caller inspects to distinguish a dangling link
/// (ignored) from a genuine escape.
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

/// Normalize a bundle-relative path so snapshot keys use forward slashes.
///
/// On Windows the native `\` separator is folded to `/`; a backslash can never
/// appear inside a Windows file name, so the replacement only ever rewrites
/// separators. On POSIX systems `/` is already the native separator and `\` is
/// a legal file-name byte, so the path is kept verbatim — rewriting it there
/// would produce a snapshot key that no longer resolves on disk when joined
/// back onto the bundle root.
#[cfg(windows)]
fn normalized_relative_path(path: &Path) -> PathBuf {
    PathBuf::from(path.to_string_lossy().replace('\\', "/"))
}

#[cfg(not(windows))]
fn normalized_relative_path(path: &Path) -> PathBuf {
    path.to_path_buf()
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

    #[cfg(unix)]
    #[test]
    fn a_posix_filename_containing_a_backslash_is_preserved_verbatim() {
        let root = TempDir::new().unwrap();
        write_file(&root, r"back\slash.md", "backslash body");
        let config = SyncConfig::new(root.path());

        let snapshot = discover(&config).unwrap();

        assert_eq!(snapshot.len(), 1);
        let file = snapshot.get(r"back\slash.md").unwrap();
        assert_eq!(file.hash, crate::hash_bytes(b"backslash body"));
        // The snapshot key must join back onto the root to an existing file,
        // otherwise consumers reading `root.join(key)` fail on a phantom path.
        assert!(root.path().join(&file.path).is_file());
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
