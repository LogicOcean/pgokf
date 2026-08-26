//! Filesystem-backed incremental synchronization for Open Knowledge Format bundles.
//!
//! This crate deliberately has no database dependency. It discovers Markdown files,
//! snapshots their contents and metadata, then compares snapshots to produce a
//! deterministic [`SyncPlan`]. Application binaries can initialize configuration and
//! logging with `stratify`; the planner itself is synchronous and side-effect free
//! apart from reading its configured bundle directory.

use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
    time::SystemTime,
};

use globset::{Glob, GlobSet, GlobSetBuilder};
use thiserror::Error;
use walkdir::WalkDir;

/// Re-export the service bootstrap used by OKF applications for configuration and
/// logging. Callers should initialize it before invoking [`discover`] when they want
/// the planner's `tracing` diagnostics to be collected.
pub use stratify;

/// Configuration for a filesystem scan.
///
/// `include` and `exclude` are glob patterns relative to [`Self::root`]. When no
/// include patterns are supplied, all `**/*.md` files are included. Excludes always
/// take precedence over includes.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SyncConfig {
    pub root: PathBuf,
    pub include: Vec<String>,
    pub exclude: Vec<String>,
}

impl SyncConfig {
    #[must_use]
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            include: vec!["**/*.md".to_owned()],
            exclude: Vec::new(),
        }
    }

    #[must_use]
    pub fn with_include(mut self, patterns: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.include = patterns.into_iter().map(Into::into).collect();
        self
    }

    #[must_use]
    pub fn with_exclude(mut self, patterns: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.exclude = patterns.into_iter().map(Into::into).collect();
        self
    }
}

/// Content and filesystem attributes captured for one document.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FileMetadata {
    /// Path relative to the configured root, using forward slashes.
    pub path: PathBuf,
    /// Lowercase hexadecimal BLAKE3 hash of the file contents.
    pub hash: String,
    pub size_bytes: u64,
    pub modified_at: Option<SystemTime>,
}

/// A point-in-time view of all discovered files, indexed by normalized relative path.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Snapshot {
    files: BTreeMap<PathBuf, FileMetadata>,
}

impl Snapshot {
    #[must_use]
    pub fn files(&self) -> &BTreeMap<PathBuf, FileMetadata> {
        &self.files
    }

    #[must_use]
    pub fn get(&self, path: impl AsRef<Path>) -> Option<&FileMetadata> {
        self.files.get(path.as_ref())
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.files.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.files.is_empty()
    }
}

/// A file whose current contents differ from the preceding snapshot.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UpdatedFile {
    pub previous: FileMetadata,
    pub current: FileMetadata,
}

/// The work required to move a persisted snapshot to a newly discovered snapshot.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct SyncPlan {
    pub added: Vec<FileMetadata>,
    pub updated: Vec<UpdatedFile>,
    pub removed: Vec<FileMetadata>,
    pub unchanged: Vec<FileMetadata>,
}

impl SyncPlan {
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.added.is_empty() && self.updated.is_empty() && self.removed.is_empty()
    }
}

#[derive(Debug, Error)]
pub enum SyncError {
    #[error("invalid {kind} glob `{pattern}`: {source}")]
    InvalidGlob {
        kind: &'static str,
        pattern: String,
        #[source]
        source: globset::Error,
    },
    #[error("failed to inspect `{path}`: {source}")]
    Metadata {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to read `{path}`: {source}")]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to walk `{path}`: {source}")]
    Walk {
        path: PathBuf,
        #[source]
        source: walkdir::Error,
    },
    #[error("`{path}` is outside sync root `{root}`")]
    OutsideRoot { path: PathBuf, root: PathBuf },
}

/// Discover Markdown files under `config.root` and calculate their BLAKE3 hashes.
///
/// # Errors
///
/// Returns [`SyncError`] if a glob pattern is invalid, a file cannot be read, or the directory walk fails.
pub fn discover(config: &SyncConfig) -> Result<Snapshot, SyncError> {
    let includes = build_glob_set(&config.include, "include")?;
    let excludes = build_glob_set(&config.exclude, "exclude")?;
    let mut files = BTreeMap::new();

    for entry in WalkDir::new(&config.root).follow_links(false) {
        let entry = entry.map_err(|source| SyncError::Walk {
            path: config.root.clone(),
            source,
        })?;
        if !entry.file_type().is_file() {
            continue;
        }

        let absolute = entry.path();
        let relative = absolute
            .strip_prefix(&config.root)
            .map_err(|_| SyncError::OutsideRoot {
                path: absolute.to_path_buf(),
                root: config.root.clone(),
            })?;
        let path = normalized_relative_path(relative);

        // Guarding on the extension makes the Markdown-only contract independent of
        // whether a caller supplies a broad include such as `docs/**`.
        if path.extension().and_then(|extension| extension.to_str()) != Some("md")
            || (!config.include.is_empty() && !includes.is_match(&path))
            || excludes.is_match(&path)
        {
            continue;
        }

        let metadata = fs::metadata(absolute).map_err(|source| SyncError::Metadata {
            path: absolute.to_path_buf(),
            source,
        })?;
        let contents = fs::read(absolute).map_err(|source| SyncError::Read {
            path: absolute.to_path_buf(),
            source,
        })?;
        let file = FileMetadata {
            path: path.clone(),
            hash: blake3::hash(&contents).to_hex().to_string(),
            size_bytes: metadata.len(),
            modified_at: metadata.modified().ok(),
        };
        tracing::debug!(path = %path.display(), hash = %file.hash, "discovered OKF document");
        files.insert(path, file);
    }

    Ok(Snapshot { files })
}

/// Compare `previous` to `current` and construct deterministic incremental work.
///
/// A metadata-only change is unchanged: the BLAKE3 hash is the content identity used
/// to decide whether downstream consumers need to synchronize a document.
#[must_use]
pub fn plan(previous: &Snapshot, current: &Snapshot) -> SyncPlan {
    let paths: BTreeSet<_> = previous.files.keys().chain(current.files.keys()).collect();
    let mut result = SyncPlan::default();

    for path in paths {
        match (previous.files.get(path), current.files.get(path)) {
            (None, Some(current)) => result.added.push(current.clone()),
            (Some(previous), None) => result.removed.push(previous.clone()),
            (Some(previous), Some(current)) if previous.hash == current.hash => {
                result.unchanged.push(current.clone());
            }
            (Some(previous), Some(current)) => result.updated.push(UpdatedFile {
                previous: previous.clone(),
                current: current.clone(),
            }),
            (None, None) => unreachable!("path comes from at least one snapshot"),
        }
    }
    result
}

/// Discover the current filesystem state and compare it to `previous` in one call.
///
/// # Errors
///
/// Returns [`SyncError`] if the discovery step fails; see [`discover`].
pub fn build_plan(
    config: &SyncConfig,
    previous: &Snapshot,
) -> Result<(Snapshot, SyncPlan), SyncError> {
    let current = discover(config)?;
    let sync_plan = plan(previous, &current);
    Ok((current, sync_plan))
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

fn normalized_relative_path(path: &Path) -> PathBuf {
    PathBuf::from(path.to_string_lossy().replace('\\', "/"))
}

#[cfg(test)]
mod tests {
    use std::{fs, thread, time::Duration};

    use super::*;
    use tempfile::TempDir;

    fn write(root: &TempDir, relative: &str, contents: &str) {
        let path = root.path().join(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, contents).unwrap();
    }

    #[test]
    fn discovers_only_markdown_matching_the_globs() {
        let root = TempDir::new().unwrap();
        write(&root, "guide/keep.md", "keep");
        write(&root, "guide/private.md", "hidden");
        write(&root, "guide/not-a-document.txt", "nope");
        let config = SyncConfig::new(root.path())
            .with_include(["guide/**/*.md"])
            .with_exclude(["**/private.md"]);

        let snapshot = discover(&config).unwrap();
        assert_eq!(snapshot.len(), 1);
        let file = snapshot.get("guide/keep.md").unwrap();
        assert_eq!(file.hash, blake3::hash(b"keep").to_hex().to_string());
        assert_eq!(file.size_bytes, 4);
        assert!(file.modified_at.is_some());
    }

    #[test]
    fn an_empty_include_list_selects_all_markdown_files() {
        let root = TempDir::new().unwrap();
        write(&root, "root.md", "root");
        write(&root, "nested/document.md", "nested");
        write(&root, "nested/ignored.txt", "not markdown");
        let config = SyncConfig::new(root.path()).with_include(Vec::<String>::new());

        let snapshot = discover(&config).unwrap();

        assert_eq!(snapshot.len(), 2);
        assert!(snapshot.get("root.md").is_some());
        assert!(snapshot.get("nested/document.md").is_some());
    }

    #[test]
    fn incremental_plan_classifies_added_updated_removed_and_unchanged() {
        let root = TempDir::new().unwrap();
        write(&root, "unchanged.md", "same");
        write(&root, "updated.md", "before");
        write(&root, "removed.md", "gone");
        let config = SyncConfig::new(root.path());
        let before = discover(&config).unwrap();

        write(&root, "updated.md", "after");
        fs::remove_file(root.path().join("removed.md")).unwrap();
        write(&root, "added.md", "new");
        let after = discover(&config).unwrap();
        let sync_plan = plan(&before, &after);

        assert_eq!(
            sync_plan.added.iter().map(|f| &f.path).collect::<Vec<_>>(),
            vec![&PathBuf::from("added.md")]
        );
        assert_eq!(sync_plan.updated.len(), 1);
        assert_eq!(
            sync_plan.updated[0].current.path,
            PathBuf::from("updated.md")
        );
        assert_ne!(
            sync_plan.updated[0].previous.hash,
            sync_plan.updated[0].current.hash
        );
        assert_eq!(sync_plan.removed[0].path, PathBuf::from("removed.md"));
        assert_eq!(sync_plan.unchanged[0].path, PathBuf::from("unchanged.md"));
        assert!(!sync_plan.is_empty());
    }

    #[test]
    fn metadata_only_changes_do_not_create_updates() {
        let root = TempDir::new().unwrap();
        write(&root, "doc.md", "stable");
        let config = SyncConfig::new(root.path());
        let before = discover(&config).unwrap();
        thread::sleep(Duration::from_millis(5));
        write(&root, "doc.md", "stable");
        let after = discover(&config).unwrap();

        let sync_plan = plan(&before, &after);
        assert!(sync_plan.is_empty());
        assert_eq!(sync_plan.unchanged.len(), 1);
    }

    #[test]
    fn build_plan_returns_the_snapshot_to_persist_for_the_next_sync() {
        let root = TempDir::new().unwrap();
        let config = SyncConfig::new(root.path());
        let empty = Snapshot::default();
        write(&root, "document.md", "first version");

        let (first_snapshot, first_plan) = build_plan(&config, &empty).unwrap();
        assert_eq!(first_plan.added.len(), 1);

        write(&root, "document.md", "second version");
        let (_, second_plan) = build_plan(&config, &first_snapshot).unwrap();
        assert_eq!(second_plan.updated.len(), 1);
        assert_eq!(second_plan.added.len(), 0);
    }
}
