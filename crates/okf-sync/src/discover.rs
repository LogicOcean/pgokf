// SPDX-License-Identifier: AGPL-3.0-only

use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    time::SystemTime,
};

use globset::{Glob, GlobSet, GlobSetBuilder};
use walkdir::WalkDir;

use crate::{FileClass, PackageIndex, SyncConfig, SyncError, hash::hash_file};

/// Content and filesystem attributes captured for one discovered file.
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
    /// What the file is to the catalog: a document, a skill manifest, a
    /// package resource, or a reserved bookkeeping file.
    pub class: FileClass,
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

/// Discover the catalog content under `config.root` and calculate its BLAKE3
/// hashes: every Markdown document selected by the globs, plus every resource
/// of a skill package whose `SKILL.md` is selected.
///
/// Discovery walks the tree twice. The first pass only locates skill packages
/// (a regular file named exactly `SKILL.md` that the include and exclude
/// globs select), which costs one path per package and no reads. The second
/// pass classifies every file against those packages with
/// [`PackageIndex::classify`] and reads the ones that are content: Markdown
/// documents and manifests must match the include globs as they always have;
/// a resource below a package's `scripts/`, `references/`, or `assets/` is
/// content because its manifest was selected, whatever its extension, unless
/// an exclude glob removes it. Excludes always win.
///
/// Symbolic links are never followed while walking. Containment is enforced only on
/// links that are themselves candidate content - a name the rules above select -
/// because only those could contribute content. A candidate link whose target stays
/// inside the bundle root is skipped, since that target is discovered at its own
/// canonical path; a candidate link whose target resolves outside the root is
/// rejected so a bundle can never pull content from elsewhere on the filesystem.
/// Any other link - not content, excluded, or dangling - is ignored without being
/// resolved, so a stray or broken link anywhere in the tree can never abort
/// discovery. A symlinked `SKILL.md` declares no package, for the same reason.
///
/// When [`SyncConfig::max_file_bytes`] is set, each candidate's size is checked via
/// metadata before its contents are read; when [`SyncConfig::max_files`] is set the
/// scan stops as soon as one more file than the limit is found. Both limits count
/// documents and package resources alike.
///
/// # Errors
///
/// Returns [`SyncError`] if a glob pattern is invalid, the walk fails, a file cannot
/// be inspected or read, a candidate symlink escapes the root, or a configured
/// resource limit is exceeded.
pub fn discover(config: &SyncConfig) -> Result<Snapshot, SyncError> {
    let includes = build_glob_set(&config.include, "include")?;
    let excludes = build_glob_set(&config.exclude, "exclude")?;
    let rules = CandidateRules {
        config,
        includes: &includes,
        excludes: &excludes,
        packages: locate_packages(config, &includes, &excludes)?,
    };
    let mut files = BTreeMap::new();
    let mut total_bytes: u64 = 0;

    for entry in WalkDir::new(&config.root).follow_links(false) {
        let entry = entry.map_err(|source| SyncError::Walk {
            path: config.root.clone(),
            source,
        })?;
        let absolute = entry.path();
        let path = bundle_relative_path(absolute, &config.root)?;

        if entry.path_is_symlink() {
            check_candidate_symlink(absolute, &path, &rules)?;
            // A resolvable in-root link contributes nothing new: its target is
            // discovered at the target's own canonical path.
            continue;
        }
        if !entry.file_type().is_file() {
            continue;
        }
        let Some(class) = rules.classify(&path) else {
            continue;
        };

        if let Some(limit) = config.max_files
            && files.len() >= limit
        {
            return Err(SyncError::TooManyFiles {
                count: files.len() + 1,
                limit,
            });
        }
        let file = read_candidate_file(absolute, &path, config, class)?;
        if let Some(limit) = config.max_total_bytes {
            total_bytes = total_bytes.saturating_add(file.size_bytes);
            if total_bytes > limit {
                return Err(SyncError::BundleTooLarge {
                    total_bytes,
                    limit_bytes: limit,
                });
            }
        }
        tracing::debug!(path = %path.display(), hash = %file.hash, class = class.label(), "discovered catalog file");
        files.insert(path, file);
    }

    Ok(Snapshot::new(files))
}

/// First pass: the directories of every regular `SKILL.md` the globs select.
fn locate_packages(
    config: &SyncConfig,
    includes: &GlobSet,
    excludes: &GlobSet,
) -> Result<PackageIndex, SyncError> {
    // Collected, then indexed in one pass: a manifest nested in another
    // package's resource directory is that package's resource, and deciding
    // that needs the enclosing packages known first.
    let mut manifests: Vec<String> = Vec::new();
    for entry in WalkDir::new(&config.root).follow_links(false) {
        let entry = entry.map_err(|source| SyncError::Walk {
            path: config.root.clone(),
            source,
        })?;
        if !entry.file_type().is_file() {
            continue;
        }
        let path = bundle_relative_path(entry.path(), &config.root)?;
        let text = path.to_string_lossy();
        if PackageIndex::manifest_directory(&text).is_some()
            && selected_by_globs(&path, config, includes, excludes)
        {
            manifests.push(text.into_owned());
        }
    }
    Ok(PackageIndex::from_paths(
        manifests.iter().map(String::as_str),
    ))
}

/// The scan's selection rules: globs plus the packages of this snapshot.
struct CandidateRules<'a> {
    config: &'a SyncConfig,
    includes: &'a GlobSet,
    excludes: &'a GlobSet,
    packages: PackageIndex,
}

impl CandidateRules<'_> {
    /// Classify a bundle-relative path, or `None` when it is not content to
    /// discover.
    ///
    /// Membership depends only on the path, so it can be decided for a symlink
    /// without resolving its target. A document or manifest must be selected
    /// by the include globs (the historical Markdown-only contract, independent
    /// of whether a caller supplies a broad include such as `docs/**`); a
    /// package resource is selected through its manifest. Excludes remove
    /// anything.
    fn classify(&self, path: &Path) -> Option<FileClass> {
        if self.excludes.is_match(path) {
            return None;
        }
        let class = self.packages.classify(&path.to_string_lossy())?;
        if class.is_resource() || self.config.include.is_empty() || self.includes.is_match(path) {
            Some(class)
        } else {
            None
        }
    }
}

/// Whether the include and exclude globs select a path.
fn selected_by_globs(
    path: &Path,
    config: &SyncConfig,
    includes: &GlobSet,
    excludes: &GlobSet,
) -> bool {
    (config.include.is_empty() || includes.is_match(path)) && !excludes.is_match(path)
}

/// Map an absolute walked path to its normalized bundle-relative snapshot key.
fn bundle_relative_path(absolute: &Path, root: &Path) -> Result<PathBuf, SyncError> {
    let relative = absolute
        .strip_prefix(root)
        .map_err(|_| SyncError::OutsideRoot {
            path: absolute.to_path_buf(),
            root: root.to_path_buf(),
        })?;
    Ok(normalized_relative_path(relative))
}

/// Enforce root containment for a symlink, but only when it is candidate
/// content.
///
/// Containment is a contract on candidate content only: a link that is not
/// selectable - not a document, manifest, or package resource, or excluded - is
/// ignored without being resolved, so a stray link elsewhere in the tree can
/// never abort discovery. A candidate link that dangles (its target resolves to
/// nothing) has no in-bundle content to index and nothing to escape, so it too
/// is accepted.
fn check_candidate_symlink(
    absolute: &Path,
    path: &Path,
    rules: &CandidateRules<'_>,
) -> Result<(), SyncError> {
    if rules.classify(path).is_none() {
        return Ok(());
    }
    match ensure_symlink_containment(absolute, &rules.config.root) {
        Ok(()) => Ok(()),
        Err(SyncError::Metadata { source, .. })
            if source.kind() == std::io::ErrorKind::NotFound =>
        {
            Ok(())
        }
        Err(other) => Err(other),
    }
}

/// Read a candidate file's metadata (enforcing [`SyncConfig::max_file_bytes`]
/// before reading its contents) and BLAKE3 hash into a [`FileMetadata`].
fn read_candidate_file(
    absolute: &Path,
    path: &Path,
    config: &SyncConfig,
    class: FileClass,
) -> Result<FileMetadata, SyncError> {
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
    Ok(FileMetadata {
        path: path.to_path_buf(),
        hash: hash_file(absolute)?,
        size_bytes: metadata.len(),
        modified_at: metadata.modified().ok(),
        class,
    })
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

/// Reject a candidate symlink whose fully resolved target lies outside the bundle
/// root.
///
/// Only candidate links reach this check: an escaping file link would otherwise
/// let [`hash_file`] read content from outside the bundle. A target that cannot
/// be resolved surfaces as a [`SyncError::Metadata`] error carrying the
/// underlying I/O error, which the caller inspects to distinguish a dangling
/// link (ignored) from a genuine escape.
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
/// a legal file-name byte, so the path is kept verbatim - rewriting it there
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
    fn a_bundle_whose_files_add_up_past_max_total_bytes_is_rejected() {
        // Arrange: two files, each under the per-file ceiling, together over
        // the total - which is the shape that matters now that a package
        // resource is stored whole whatever its type.
        let root = TempDir::new().unwrap();
        write_file(&root, "a.md", "aaaaaaaa");
        write_file(&root, "b.md", "bbbbbbbb");
        let config = SyncConfig::new(root.path())
            .with_max_file_bytes(64)
            .with_max_total_bytes(12);

        // Act
        let error = discover(&config).expect_err("refused");

        // Assert
        assert!(
            matches!(
                error,
                SyncError::BundleTooLarge {
                    limit_bytes: 12,
                    ..
                }
            ),
            "{error}"
        );
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

    #[test]
    fn a_skill_package_contributes_its_manifest_and_resources() {
        // Arrange: one package with all three resource directories, a nested
        // resource, a non-Markdown file outside the package, and a stray
        // file directly under the package root.
        let root = TempDir::new().unwrap();
        write_file(&root, "skills/deploy/SKILL.md", "---\nname: deploy\n---\n");
        write_file(&root, "skills/deploy/scripts/run.sh", "#!/bin/sh\n");
        write_file(&root, "skills/deploy/scripts/lib/util.py", "print(1)\n");
        write_file(&root, "skills/deploy/references/guide.md", "# Guide\n");
        write_file(&root, "skills/deploy/assets/diagram.png", "PNG");
        write_file(&root, "skills/deploy/notes.txt", "not content");
        write_file(&root, "tools/loose.sh", "not content either");
        let config = SyncConfig::new(root.path());

        // Act
        let snapshot = discover(&config).unwrap();

        // Assert
        let classes: Vec<(String, FileClass)> = snapshot
            .files()
            .values()
            .map(|f| (f.path.to_string_lossy().into_owned(), f.class))
            .collect();
        assert_eq!(
            classes,
            [
                (
                    "skills/deploy/SKILL.md".to_owned(),
                    FileClass::SkillManifest
                ),
                (
                    "skills/deploy/assets/diagram.png".to_owned(),
                    FileClass::SkillAsset
                ),
                (
                    "skills/deploy/references/guide.md".to_owned(),
                    FileClass::SkillReference
                ),
                (
                    "skills/deploy/scripts/lib/util.py".to_owned(),
                    FileClass::SkillScript
                ),
                (
                    "skills/deploy/scripts/run.sh".to_owned(),
                    FileClass::SkillScript
                ),
            ]
        );
        let script = snapshot.get("skills/deploy/scripts/run.sh").unwrap();
        assert_eq!(script.hash, crate::hash_bytes(b"#!/bin/sh\n"));
        assert_eq!(script.size_bytes, 10);
    }

    #[test]
    fn an_excluded_manifest_declares_no_package() {
        // Arrange: the manifest is excluded, so its resources are ordinary
        // non-content files and its Markdown reference is a plain document.
        let root = TempDir::new().unwrap();
        write_file(&root, "pkg/SKILL.md", "---\nname: pkg\n---\n");
        write_file(&root, "pkg/scripts/run.sh", "x");
        write_file(&root, "pkg/references/guide.md", "# Guide\n");
        let config = SyncConfig::new(root.path()).with_exclude(["**/SKILL.md"]);

        // Act
        let snapshot = discover(&config).unwrap();

        // Assert
        assert_eq!(snapshot.len(), 1);
        assert_eq!(
            snapshot.get("pkg/references/guide.md").unwrap().class,
            FileClass::OkfDocument
        );
    }

    #[test]
    fn an_excluded_resource_is_not_discovered() {
        // Arrange
        let root = TempDir::new().unwrap();
        write_file(&root, "pkg/SKILL.md", "---\nname: pkg\n---\n");
        write_file(&root, "pkg/scripts/run.sh", "x");
        write_file(&root, "pkg/assets/node_modules/dep.js", "y");
        let config = SyncConfig::new(root.path()).with_exclude(["**/node_modules/**"]);

        // Act
        let snapshot = discover(&config).unwrap();

        // Assert
        assert_eq!(snapshot.len(), 2);
        assert!(snapshot.get("pkg/assets/node_modules/dep.js").is_none());
    }

    #[test]
    fn include_globs_select_manifests_but_never_filter_resources() {
        // Arrange: a narrow include that matches the manifest but not the
        // resource paths; the resources are still selected through it.
        let root = TempDir::new().unwrap();
        write_file(&root, "pkg/SKILL.md", "---\nname: pkg\n---\n");
        write_file(&root, "pkg/scripts/run.sh", "x");
        write_file(&root, "other/SKILL.md", "---\nname: other\n---\n");
        write_file(&root, "other/scripts/run.sh", "y");
        let config = SyncConfig::new(root.path()).with_include(["pkg/**/*.md"]);

        // Act
        let snapshot = discover(&config).unwrap();

        // Assert
        let paths: Vec<String> = snapshot
            .files()
            .keys()
            .map(|p| p.to_string_lossy().into_owned())
            .collect();
        assert_eq!(paths, ["pkg/SKILL.md", "pkg/scripts/run.sh"]);
    }

    #[test]
    fn resources_count_toward_the_file_and_size_limits() {
        // Arrange: the manifest is comfortably under the ceiling and the
        // resource is over it, so the resource is the only file that can
        // trip it - whichever order the walk reaches them in.
        let root = TempDir::new().unwrap();
        let manifest = "---\nname: pkg\n---\n";
        assert!(manifest.len() < 20, "the manifest must not trip the limit");
        write_file(&root, "pkg/SKILL.md", manifest);
        write_file(&root, "pkg/assets/big.bin", &"0".repeat(30));
        let config = SyncConfig::new(root.path()).with_max_file_bytes(20);

        // Act
        let result = discover(&config);

        // Assert
        assert!(
            matches!(
                &result,
                Err(SyncError::FileTooLarge {
                    size_bytes: 30,
                    limit_bytes: 20,
                    path,
                }) if path.ends_with("big.bin")
            ),
            "{result:?}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_symlinked_manifest_declares_no_package_and_an_escaping_resource_link_is_rejected() {
        // Arrange: an in-root symlinked SKILL.md is skipped like every symlink
        // (its target is discovered at its own path), so the directory is not
        // a package and its scripts are not content; a real manifest with a
        // resource link that escapes the root is rejected.
        let root = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        write_file(&root, "docs/real.md", "---\ntype: Note\ntitle: Real\n---\n");
        write_file(&outside, "secret.sh", "echo secret");
        fs::create_dir_all(root.path().join("linked/scripts")).unwrap();
        std::os::unix::fs::symlink(
            root.path().join("docs/real.md"),
            root.path().join("linked/SKILL.md"),
        )
        .unwrap();
        write_file(&root, "linked/scripts/run.sh", "x");
        let config = SyncConfig::new(root.path());

        // Act
        let snapshot = discover(&config).unwrap();

        // Assert: only the real document is content.
        assert_eq!(snapshot.len(), 1);
        assert!(snapshot.get("docs/real.md").is_some());

        // Arrange: now a real package whose scripts/ holds an escaping link.
        write_file(&root, "pkg/SKILL.md", "---\nname: pkg\n---\n");
        fs::create_dir_all(root.path().join("pkg/scripts")).unwrap();
        std::os::unix::fs::symlink(
            outside.path().join("secret.sh"),
            root.path().join("pkg/scripts/secret.sh"),
        )
        .unwrap();

        // Act
        let result = discover(&config);

        // Assert
        assert!(matches!(result, Err(SyncError::SymlinkEscape { .. })));
    }
}
