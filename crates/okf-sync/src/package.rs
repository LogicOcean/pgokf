// SPDX-License-Identifier: AGPL-3.0-only
//! Skill-package ownership and source-file classification.
//!
//! An Agent Skills package is a directory holding a `SKILL.md` manifest and,
//! optionally, `scripts/`, `references/`, and `assets/` subdirectories. Bundle
//! discovery has to know about packages because their resources are not
//! Markdown documents: a shell script or a PNG diagram under a package is
//! catalog content that must be captured byte for byte, while the same file
//! anywhere else in the bundle is simply not an OKF document.
//!
//! [`PackageIndex`] holds the package roots of one snapshot and answers, for
//! any bundle-relative path, which package owns it and which [`FileClass`] it
//! has. Classification is deterministic and follows the precedence of the
//! extension specification (§5.4), **ownership first**:
//!
//! 1. inside the nearest enclosing package's `scripts/`, `references/`, or
//!    `assets/` directory, a file is that package's [`FileClass::SkillScript`],
//!    [`FileClass::SkillReference`], or [`FileClass::SkillAsset`] - whatever it
//!    is called, so a `references/index.md` is a reference and a nested
//!    `references/SKILL.md` is a reference too, not a second package;
//! 2. otherwise a reserved OKF basename (`index.md` / `log.md`) is
//!    [`FileClass::Reserved`];
//! 3. otherwise the exact, case-sensitive basename `SKILL.md` is a
//!    [`FileClass::SkillManifest`], opening a package;
//! 4. otherwise any `.md` file is an [`FileClass::OkfDocument`];
//! 5. everything else is ignored (it never enters a snapshot).
//!
//! Everything under a package's resource directories belongs to that package,
//! so a nested `SKILL.md` inside another package's `scripts/`/`references/`/
//! `assets/` is a resource of the enclosing one, not a new boundary; a
//! `SKILL.md` elsewhere opens its own package. A path never has two owners.

use std::collections::BTreeSet;

/// The Agent Skills manifest file name, matched case-sensitively.
pub const SKILL_MANIFEST: &str = "SKILL.md";

/// The three resource directories an Agent Skills package may carry.
pub const RESOURCE_DIRECTORIES: [&str; 3] = ["scripts", "references", "assets"];

/// File names reserved by OKF; they describe a directory, not a concept.
const RESERVED_FILE_NAMES: [&str; 2] = ["index.md", "log.md"];

/// What one snapshot entry is to the catalog.
///
/// Every entry of a snapshot carries exactly one class; files that classify
/// to none are never discovered.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, PartialOrd, Ord)]
pub enum FileClass {
    /// An ordinary OKF Markdown document, parsed for its frontmatter.
    OkfDocument,
    /// A `SKILL.md` Agent Skills manifest: the root of a skill package.
    SkillManifest,
    /// A file below an owning package's `scripts/` directory.
    SkillScript,
    /// A file below an owning package's `references/` directory.
    SkillReference,
    /// A file below an owning package's `assets/` directory.
    SkillAsset,
    /// A reserved OKF file (`index.md` / `log.md`): bookkeeping, never a
    /// concept. Kept in the snapshot so the bundle version and activity logs
    /// can be read from it.
    Reserved,
}

impl FileClass {
    /// Whether this class is a skill-package resource (a script, reference,
    /// or asset) rather than a document or manifest.
    #[must_use]
    pub const fn is_resource(self) -> bool {
        matches!(
            self,
            Self::SkillScript | Self::SkillReference | Self::SkillAsset
        )
    }

    /// The stable lowercase label used in hashes and stored metadata.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::OkfDocument => "document",
            Self::SkillManifest => "skill",
            Self::SkillScript => "script",
            Self::SkillReference => "reference",
            Self::SkillAsset => "asset",
            Self::Reserved => "reserved",
        }
    }

    /// The catalog concept ID a file of this class gets from its normalized
    /// bundle-relative path.
    ///
    /// Documents and manifests keep the historical convention (the path
    /// without its `.md` suffix). Resources are identified by their full path,
    /// extension included: `scripts/check.sh` and `scripts/check.py` are
    /// distinct files and must stay distinct concepts, and a `.md` reference
    /// must not collide with a document of the same stem elsewhere in the
    /// package.
    #[must_use]
    pub fn concept_id(self, normalized_path: &str) -> String {
        if self.is_resource() {
            normalized_path.to_owned()
        } else {
            normalized_path
                .strip_suffix(".md")
                .unwrap_or(normalized_path)
                .to_owned()
        }
    }
}

/// The package roots of one bundle snapshot.
///
/// A root is the bundle-relative directory of a `SKILL.md`, forward-slash
/// separated without a trailing slash; the empty string is the bundle root
/// itself (a bundle that *is* one package).
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct PackageIndex {
    roots: BTreeSet<String>,
}

impl PackageIndex {
    /// Index the packages declared by a set of bundle-relative paths: every
    /// path whose basename is exactly `SKILL.md` contributes its directory,
    /// unless that manifest is itself a resource of a package already
    /// indexed (see [`PackageIndex::insert_manifest`]).
    pub fn from_paths<'a>(paths: impl IntoIterator<Item = &'a str>) -> Self {
        let mut manifests: Vec<&str> = paths
            .into_iter()
            .filter(|path| Self::manifest_directory(path).is_some())
            .collect();
        // Shortest first, so an enclosing package is always decided before
        // anything nested inside it.
        manifests.sort_unstable_by_key(|path| (path.len(), *path));
        let mut index = Self::default();
        for path in manifests {
            index.insert_manifest(path);
        }
        index
    }

    /// Register the package a `SKILL.md` declares, and say whether it was.
    ///
    /// A `SKILL.md` under another package's `scripts/`, `references/` or
    /// `assets/` is **that package's resource**, not a new package. Taking
    /// it for a root would make every sibling file package-relative to it,
    /// and since those siblings are then not under a resource directory of
    /// their own they would stop being package members at all - a one-file
    /// commit quietly removing a subtree from the catalog. Callers that
    /// build an index incrementally must add manifests shortest path first,
    /// which [`PackageIndex::from_paths`] does.
    pub fn insert_manifest(&mut self, path: &str) -> bool {
        let Some(directory) = Self::manifest_directory(path) else {
            return false;
        };
        if self.resource_of_indexed_package(path) {
            return false;
        }
        self.roots.insert(directory.to_owned());
        true
    }

    /// Whether an already-indexed package owns `path` as one of its
    /// resources.
    fn resource_of_indexed_package(&self, path: &str) -> bool {
        self.owner_of(path)
            .is_some_and(|root| resource_class(root, path).is_some())
    }

    /// The indexed package roots in path order.
    pub fn roots(&self) -> impl Iterator<Item = &str> {
        self.roots.iter().map(String::as_str)
    }

    /// Whether the snapshot declares any package at all.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.roots.is_empty()
    }

    /// The directory of a path whose basename is the manifest, if it is one.
    #[must_use]
    pub fn manifest_directory(path: &str) -> Option<&str> {
        let (directory, name) = split_parent(path);
        (name == SKILL_MANIFEST).then_some(directory)
    }

    /// The root of the nearest package that encloses `path` (the manifest's
    /// own directory for a manifest), or `None` outside every package.
    #[must_use]
    pub fn owner_of(&self, path: &str) -> Option<&str> {
        let (mut directory, _) = split_parent(path);
        loop {
            if let Some(root) = self.roots.get(directory) {
                return Some(root.as_str());
            }
            if directory.is_empty() {
                return None;
            }
            directory = split_parent(directory).0;
        }
    }

    /// Classify one bundle-relative path; `None` means the file is not
    /// catalog content and must not be discovered.
    #[must_use]
    pub fn classify(&self, path: &str) -> Option<FileClass> {
        // Ownership first: inside a package's `scripts/`, `references/` or
        // `assets/` every file is that package's resource, whatever it is
        // called. A `references/index.md` is an ordinary reference, and a
        // `references/SKILL.md` is a reference too, not a second package.
        if let Some(root) = self.owner_of(path)
            && let Some(class) = resource_class(root, path)
        {
            return Some(class);
        }
        let (_, name) = split_parent(path);
        if RESERVED_FILE_NAMES.contains(&name) {
            return Some(FileClass::Reserved);
        }
        if name == SKILL_MANIFEST {
            return Some(FileClass::SkillManifest);
        }
        is_markdown(name).then_some(FileClass::OkfDocument)
    }

    /// The path of a resource relative to its owning package (`scripts/x.sh`
    /// for `tools/foo/scripts/x.sh` owned by `tools/foo`), or `None` when the
    /// path is not a resource of any package.
    #[must_use]
    pub fn package_relative<'p>(&self, path: &'p str) -> Option<&'p str> {
        let root = self.owner_of(path)?;
        let relative = strip_root(root, path)?;
        resource_class(root, path).map(|_| relative)
    }
}

/// The resource class of `path` inside the package rooted at `root`, if its
/// first package-relative segment is a resource directory with something
/// below it.
fn resource_class(root: &str, path: &str) -> Option<FileClass> {
    let relative = strip_root(root, path)?;
    let (directory, _) = relative.split_once('/')?;
    match directory {
        "scripts" => Some(FileClass::SkillScript),
        "references" => Some(FileClass::SkillReference),
        "assets" => Some(FileClass::SkillAsset),
        _ => None,
    }
}

/// `path` relative to the package root `root` (`""` is the bundle root).
fn strip_root<'p>(root: &str, path: &'p str) -> Option<&'p str> {
    if root.is_empty() {
        Some(path)
    } else {
        path.strip_prefix(root)?.strip_prefix('/')
    }
}

/// Split a forward-slash path into its parent directory (`""` at the root)
/// and basename.
fn split_parent(path: &str) -> (&str, &str) {
    path.rsplit_once('/').unwrap_or(("", path))
}

/// Whether a basename carries the exact lowercase `.md` extension the
/// document scan has always required.
fn is_markdown(name: &str) -> bool {
    std::path::Path::new(name)
        .extension()
        .is_some_and(|extension| extension == "md")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_manifest_inside_another_package_is_that_package_s_resource() {
        // Arrange: a SKILL.md committed under a package's references/ used
        // to become a package root of its own, which made every sibling
        // package-relative to it - and so no longer a member of anything.
        let index = PackageIndex::from_paths([
            "tools/kit/SKILL.md",
            "tools/kit/references/SKILL.md",
            "tools/kit/assets/SKILL.md",
        ]);

        // Act / Assert: one package, and the nested manifests are its
        // resources like any other file there.
        assert_eq!(index.roots().collect::<Vec<_>>(), ["tools/kit"]);
        assert_eq!(
            index.classify("tools/kit/references/SKILL.md"),
            Some(FileClass::SkillReference)
        );
        assert_eq!(
            index.classify("tools/kit/references/guide.md"),
            Some(FileClass::SkillReference)
        );
        assert_eq!(
            index.classify("tools/kit/assets/logo.png"),
            Some(FileClass::SkillAsset)
        );
        assert_eq!(
            index.classify("tools/kit/SKILL.md"),
            Some(FileClass::SkillManifest)
        );
    }

    #[test]
    fn a_package_still_nests_outside_a_resource_directory() {
        // Arrange / Act: only the three resource directories make a nested
        // manifest a resource; anywhere else it is its own package.
        let index = PackageIndex::from_paths(["tools/kit/SKILL.md", "tools/kit/inner/SKILL.md"]);

        // Assert
        assert_eq!(
            index.roots().collect::<Vec<_>>(),
            ["tools/kit", "tools/kit/inner"]
        );
        assert_eq!(
            index.classify("tools/kit/inner/scripts/run.sh"),
            Some(FileClass::SkillScript)
        );
    }

    #[test]
    fn a_reserved_name_inside_a_resource_directory_is_a_resource() {
        // Arrange: index.md and log.md are directory bookkeeping at a
        // bundle or package root, but an ordinary file inside references/.
        let index = PackageIndex::from_paths(["tools/kit/SKILL.md"]);

        // Act / Assert
        assert_eq!(
            index.classify("tools/kit/references/index.md"),
            Some(FileClass::SkillReference)
        );
        assert_eq!(
            index.classify("tools/kit/scripts/log.md"),
            Some(FileClass::SkillScript)
        );
        assert_eq!(index.classify("index.md"), Some(FileClass::Reserved));
        assert_eq!(
            index.classify("tools/kit/index.md"),
            Some(FileClass::Reserved),
            "still bookkeeping at the package root itself"
        );
    }

    fn index(paths: &[&str]) -> PackageIndex {
        PackageIndex::from_paths(paths.iter().copied())
    }

    #[test]
    fn from_paths_indexes_the_directory_of_every_manifest() {
        // Arrange
        let paths = ["a/SKILL.md", "b/c/SKILL.md", "b/notes.md", "SKILL.md"];

        // Act
        let index = index(&paths);

        // Assert
        assert_eq!(index.roots().collect::<Vec<_>>(), ["", "a", "b/c"]);
    }

    #[test]
    fn a_lowercase_manifest_name_is_not_a_manifest() {
        // Arrange / Act
        let index = index(&["a/skill.md", "b/Skill.md"]);

        // Assert
        assert!(index.is_empty());
        assert_eq!(index.classify("a/skill.md"), Some(FileClass::OkfDocument));
    }

    #[test]
    fn owner_is_the_nearest_enclosing_package() {
        // Arrange: the second manifest sits inside the first package's
        // scripts/, so it is that package's script, not a package.
        let index = index(&["a/SKILL.md", "a/scripts/inner/SKILL.md"]);

        // Act / Assert: everything under a/scripts/ belongs to a.
        assert_eq!(index.owner_of("a/scripts/run.sh"), Some("a"));
        assert_eq!(index.owner_of("a/scripts/inner/scripts/x.sh"), Some("a"));
        assert_eq!(index.owner_of("a/SKILL.md"), Some("a"));
        assert_eq!(index.owner_of("other/x.sh"), None);
    }

    #[test]
    fn classify_follows_the_specified_precedence() {
        // Arrange
        let index = index(&["pkg/SKILL.md"]);

        // Act / Assert: inside a resource directory every file is a
        // resource, reserved names included - index.md is bookkeeping at a
        // root, but an ordinary reference under references/.
        assert_eq!(
            index.classify("pkg/references/index.md"),
            Some(FileClass::SkillReference)
        );
        assert_eq!(index.classify("pkg/log.md"), Some(FileClass::Reserved));
        assert_eq!(
            index.classify("pkg/SKILL.md"),
            Some(FileClass::SkillManifest)
        );
        assert_eq!(
            index.classify("pkg/scripts/check.sh"),
            Some(FileClass::SkillScript)
        );
        assert_eq!(
            index.classify("pkg/scripts/nested/deep.py"),
            Some(FileClass::SkillScript)
        );
        assert_eq!(
            index.classify("pkg/references/guide.md"),
            Some(FileClass::SkillReference)
        );
        assert_eq!(
            index.classify("pkg/assets/topology.png"),
            Some(FileClass::SkillAsset)
        );
        // A Markdown file inside the package but outside a resource directory
        // is an ordinary document; a non-Markdown one is nothing.
        assert_eq!(index.classify("pkg/notes.md"), Some(FileClass::OkfDocument));
        assert_eq!(index.classify("pkg/notes.txt"), None);
        assert_eq!(index.classify("pkg/scripts"), None);
        // Outside any package only Markdown counts.
        assert_eq!(index.classify("docs/a.md"), Some(FileClass::OkfDocument));
        assert_eq!(index.classify("docs/scripts/a.sh"), None);
    }

    #[test]
    fn everything_under_a_resource_directory_belongs_to_the_enclosing_package() {
        // Arrange: a manifest committed inside the outer package's
        // references/. Treating it as a package of its own orphaned every
        // sibling - they were no longer under a resource directory of the
        // nested root, so they stopped being catalog content at all.
        let index = index(&["a/SKILL.md", "a/references/inner/SKILL.md"]);

        // Act / Assert: the whole subtree is references of `a`, so nothing
        // is lost, and no second package is declared.
        assert_eq!(index.roots().collect::<Vec<_>>(), ["a"]);
        assert_eq!(
            index.classify("a/references/inner/SKILL.md"),
            Some(FileClass::SkillReference)
        );
        assert_eq!(
            index.classify("a/references/inner/notes.txt"),
            Some(FileClass::SkillReference)
        );
        assert_eq!(
            index.classify("a/references/inner/scripts/x.sh"),
            Some(FileClass::SkillReference)
        );
        assert_eq!(
            index.package_relative("a/references/inner/scripts/x.sh"),
            Some("references/inner/scripts/x.sh"),
            "relative to `a`, the package it really belongs to"
        );
    }

    #[test]
    fn a_bundle_root_manifest_owns_root_resources() {
        // Arrange
        let index = index(&["SKILL.md", "docs/a.md"]);

        // Act / Assert
        assert_eq!(index.owner_of("scripts/run.sh"), Some(""));
        assert_eq!(
            index.classify("scripts/run.sh"),
            Some(FileClass::SkillScript)
        );
        assert_eq!(
            index.package_relative("scripts/run.sh"),
            Some("scripts/run.sh")
        );
        assert_eq!(index.classify("docs/a.md"), Some(FileClass::OkfDocument));
    }

    #[test]
    fn package_relative_is_none_outside_resource_directories() {
        // Arrange
        let index = index(&["pkg/SKILL.md"]);

        // Act / Assert
        assert_eq!(index.package_relative("pkg/notes.md"), None);
        assert_eq!(index.package_relative("elsewhere/scripts/x.sh"), None);
        assert_eq!(
            index.package_relative("pkg/assets/img/a.png"),
            Some("assets/img/a.png")
        );
    }

    #[test]
    fn concept_ids_keep_the_extension_for_resources_only() {
        // Arrange / Act / Assert
        assert_eq!(FileClass::OkfDocument.concept_id("docs/a.md"), "docs/a");
        assert_eq!(
            FileClass::SkillManifest.concept_id("pkg/SKILL.md"),
            "pkg/SKILL"
        );
        assert_eq!(
            FileClass::SkillReference.concept_id("pkg/references/guide.md"),
            "pkg/references/guide.md"
        );
        assert_eq!(
            FileClass::SkillScript.concept_id("pkg/scripts/run.sh"),
            "pkg/scripts/run.sh"
        );
    }

    #[test]
    fn labels_are_stable_lowercase_words() {
        // Arrange / Act / Assert
        assert_eq!(FileClass::SkillScript.label(), "script");
        assert_eq!(FileClass::SkillAsset.label(), "asset");
        assert!(FileClass::SkillAsset.is_resource());
        assert!(!FileClass::SkillManifest.is_resource());
    }
}
