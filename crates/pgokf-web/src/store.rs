// SPDX-License-Identifier: AGPL-3.0-only
//! Where a bundle's documents live and how a change reaches the catalog.
//!
//! Two kinds of bundle can be changed from the UI. A *content* bundle is
//! held by the catalog itself, so a change is a full-snapshot resync built
//! from the sources the catalog stored. A *directory* bundle is synced
//! from a directory the database server reads; when the UI can reach the
//! same directory (`--bundles-dir`, mounted read-write), a change is
//! written there, atomically and confined to the bundle, and the bundle
//! is refreshed, so the directory stays the source of truth and the edit
//! survives the next sync. A bundle from an object store is changed at its
//! source, and the UI says so.

use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};

use crate::db::{BundleFile, ContentBundle, Db, SyncOutcome};

/// Where directory bundles are reachable from this process, and the path
/// prefix the database uses for the same directory.
#[derive(Debug, Clone, Default)]
pub(crate) struct Stores {
    /// The directory bundles live under, as this process sees it.
    pub local_root: Option<PathBuf>,
    /// The same directory as the database server names it (the prefix of
    /// `pgokf.bundles.path` for bundles under it).
    pub db_root: Option<String>,
}

impl Stores {
    /// The local directory of a bundle whose database path is `db_path`,
    /// when the bundle lies under the mapped root.
    pub(crate) fn local_dir(&self, db_path: &str) -> Option<PathBuf> {
        let local_root = self.local_root.as_ref()?;
        let db_root = self.db_root.as_deref()?.trim_end_matches('/');
        let rest = db_path
            .strip_prefix(db_root)
            .filter(|rest| rest.is_empty() || rest.starts_with('/'))?
            .trim_start_matches('/');
        // The database path is trusted (an admin registered it), but it is
        // still confined to the mapped root.
        let mut dir = local_root.clone();
        for segment in rest.split('/').filter(|s| !s.is_empty()) {
            if segment == "." || segment == ".." {
                return None;
            }
            dir.push(segment);
        }
        Some(dir)
    }
}

/// How one bundle's documents are changed.
#[derive(Debug, Clone)]
pub(crate) enum DocumentStore {
    /// The catalog holds the files: a change is a full-snapshot resync.
    Content(ContentBundle),
    /// The files live in a directory this process can write: a change is
    /// written there and the bundle refreshed.
    Directory { bundle_id: i64, root: PathBuf },
}

impl DocumentStore {
    /// Whether the change goes through the catalog's stored sources (and
    /// so needs `store_source` on).
    pub(crate) fn rebuilds_from_catalog(&self) -> bool {
        matches!(self, DocumentStore::Content(_))
    }

    /// The name `register_bundle_content` keys this bundle on, when it is a
    /// content bundle: what every writer of it locks on.
    pub(crate) fn content_name(&self) -> Option<&str> {
        match self {
            DocumentStore::Content(bundle) => Some(&bundle.name),
            DocumentStore::Directory { .. } => None,
        }
    }

    /// The bundle this store writes into.
    pub(crate) const fn bundle_id(&self) -> i64 {
        match self {
            DocumentStore::Content(bundle) => bundle.id,
            DocumentStore::Directory { bundle_id, .. } => *bundle_id,
        }
    }

    /// The document's bytes as the store holds them, when it holds them
    /// itself (a directory); `None` for a content bundle, whose source the
    /// catalog returns with the concept.
    pub(crate) fn read(&self, path: &str) -> Result<Option<Vec<u8>>> {
        match self {
            DocumentStore::Content(_) => Ok(None),
            DocumentStore::Directory { root, .. } => {
                let file = confined(root, path)?;
                std::fs::read(&file)
                    .map(Some)
                    .with_context(|| format!("reading {}", file.display()))
            }
        }
    }

    /// Whether the bundle already holds a document at `path`.
    ///
    /// Used to tell adding from replacing: adding is an uploader's act,
    /// replacing an editor's.
    ///
    /// # Errors
    ///
    /// A catalog failure, or a path that escapes the bundle directory.
    pub(crate) async fn holds(&self, writer: &Db, path: &str) -> Result<bool> {
        match self {
            DocumentStore::Content(bundle) => Ok(writer
                .bundle_files(bundle.id)
                .await?
                .iter()
                .any(|f| f.path == path)),
            DocumentStore::Directory { root, .. } => {
                Ok(std::fs::symlink_metadata(confined(root, path)?).is_ok())
            }
        }
    }

    /// Apply a change: some files replaced or added, some removed.
    pub(crate) async fn apply(
        &self,
        writer: &Db,
        changes: Vec<BundleFile>,
        removals: &[String],
    ) -> Result<SyncOutcome> {
        match self {
            DocumentStore::Content(bundle) => {
                let mut files = writer.bundle_files(bundle.id).await?;
                for change in changes {
                    match files.iter_mut().find(|f| f.path == change.path) {
                        Some(existing) => existing.bytes = change.bytes,
                        None => files.push(change),
                    }
                }
                files.retain(|f| !removals.contains(&f.path));
                writer.register_content(&bundle.name, &files).await
            }
            DocumentStore::Directory { bundle_id, root } => {
                for change in &changes {
                    write_confined(root, &change.path, &change.bytes)?;
                }
                for path in removals {
                    remove_confined(root, path)?;
                }
                writer.refresh_bundle(*bundle_id).await
            }
        }
    }
}

/// The file `path` names inside `root`, refusing anything that could
/// leave it or reach a git directory: absolute paths, `..` segments,
/// `.git`, and a symbolic link anywhere along the way (a link could point
/// outside). A harmless `.` segment is dropped, as `Path::components`
/// does; other hidden directories (`.github/skills`) are ordinary.
pub(crate) fn confined(root: &Path, path: &str) -> Result<PathBuf> {
    let relative = Path::new(path);
    if relative.as_os_str().is_empty() {
        bail!("an empty path");
    }
    let mut file = root.to_path_buf();
    for component in relative.components() {
        match component {
            Component::Normal(segment) => {
                if segment == ".git" {
                    bail!("{path:?} reaches into a git directory");
                }
                file.push(segment);
                if let Ok(meta) = std::fs::symlink_metadata(&file)
                    && meta.file_type().is_symlink()
                {
                    bail!("{path:?} goes through a symbolic link");
                }
            }
            _ => bail!("{path:?} is not a plain bundle-relative path"),
        }
    }
    Ok(file)
}

/// Write a file inside the bundle: directories are created as needed,
/// the bytes go to a temporary file beside the target, and a rename makes
/// the change visible whole, never half-written.
pub(crate) fn write_confined(root: &Path, path: &str, bytes: &[u8]) -> Result<()> {
    let file = confined(root, path)?;
    let parent = file
        .parent()
        .ok_or_else(|| anyhow!("{path:?} has no parent directory"))?;
    std::fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    let name = file
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| anyhow!("{path:?} has no file name"))?;
    // Stage beside the target under a random name opened with `create_new`,
    // so a symbolic link planted at the temporary path is refused rather than
    // followed - the same guarantee `archive.rs` gives its own staging, which
    // a plain `fs::write` here did not. The random suffix keeps two concurrent
    // writers, or a stale temporary left by an earlier crash, from colliding.
    let temp = parent.join(format!(".{name}.{}.pgokf-tmp", random_suffix()?));
    if let Err(error) = write_new(&temp, bytes) {
        return Err(error).with_context(|| format!("writing {}", temp.display()));
    }
    if let Err(error) = std::fs::rename(&temp, &file) {
        let _ = std::fs::remove_file(&temp);
        return Err(error).with_context(|| format!("replacing {}", file.display()));
    }
    Ok(())
}

/// Create a fresh file and write `bytes` to it. `create_new` fails rather
/// than open an existing path, so a symbolic link planted at `path` is
/// refused instead of written through; the bytes are flushed before the
/// caller renames the file into place.
fn write_new(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)?;
    file.write_all(bytes)?;
    file.sync_all()
}

/// A short random hex string, for a staging file name that cannot collide.
fn random_suffix() -> Result<String> {
    let mut bytes = [0u8; 8];
    getrandom::fill(&mut bytes).map_err(|e| anyhow!("reading random bytes: {e}"))?;
    Ok(bytes.iter().fold(String::with_capacity(16), |mut acc, b| {
        use std::fmt::Write as _;
        let _ = write!(acc, "{b:02x}");
        acc
    }))
}

/// Remove a file inside the bundle; a file already gone is not an error.
pub(crate) fn remove_confined(root: &Path, path: &str) -> Result<()> {
    let file = confined(root, path)?;
    match std::fs::remove_file(&file) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).with_context(|| format!("removing {}", file.display())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_root(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("pgokf-store-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        dir
    }

    #[test]
    fn the_database_path_maps_onto_the_local_root_and_nowhere_else() {
        // Arrange
        let stores = Stores {
            local_root: Some(PathBuf::from("/mnt/bundles")),
            db_root: Some("/bundles/".to_owned()),
        };
        let unmapped = Stores::default();

        // Act / Assert
        assert_eq!(
            stores.local_dir("/bundles/team-docs"),
            Some(PathBuf::from("/mnt/bundles/team-docs"))
        );
        assert_eq!(
            stores.local_dir("/bundles"),
            Some(PathBuf::from("/mnt/bundles"))
        );
        assert_eq!(
            stores.local_dir("/bundles-other/x"),
            None,
            "a prefix, not a directory"
        );
        assert_eq!(stores.local_dir("/srv/x"), None);
        assert_eq!(stores.local_dir("/bundles/../etc"), None);
        assert_eq!(unmapped.local_dir("/bundles/x"), None);
    }

    #[test]
    fn paths_stay_inside_the_bundle_and_never_follow_links() {
        // Arrange
        let root = temp_root("confine");
        std::fs::create_dir_all(root.join("docs")).expect("dir");
        std::fs::write(root.join("docs/a.md"), "x").expect("file");
        let outside = temp_root("outside");
        std::os::unix::fs::symlink(&outside, root.join("link")).expect("symlink");

        // Act / Assert
        assert_eq!(
            confined(&root, "docs/a.md").expect("ok"),
            root.join("docs/a.md")
        );
        assert!(confined(&root, "../a.md").is_err());
        assert!(confined(&root, "/etc/passwd").is_err());
        assert_eq!(
            confined(&root, "docs/./a.md").expect("a harmless dot"),
            root.join("docs/a.md")
        );
        assert!(confined(&root, ".git/hooks/pre-commit.md").is_err());
        assert_eq!(
            confined(&root, ".github/skills/x/SKILL.md").expect("hidden dirs are ordinary"),
            root.join(".github/skills/x/SKILL.md")
        );
        assert!(confined(&root, "link/a.md").is_err(), "through a symlink");
        assert!(confined(&root, "").is_err());
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&outside);
    }

    #[test]
    fn writes_land_whole_and_removals_are_forgiving() {
        // Arrange
        let root = temp_root("write");

        // Act
        write_confined(&root, "runbooks/new.md", b"---\ntitle: x\n---\n").expect("writes");
        write_confined(&root, "runbooks/new.md", b"second").expect("replaces");
        remove_confined(&root, "runbooks/new.md").expect("removes");
        remove_confined(&root, "runbooks/new.md").expect("already gone");

        // Assert
        assert!(!root.join("runbooks/new.md").exists());
        assert!(
            !pgokf_tmp_files_remain(&root.join("runbooks")),
            "no staging temp file left behind"
        );
        assert!(write_confined(&root, "../escape.md", b"x").is_err());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_temp_symlink_cannot_redirect_a_write_outside_the_bundle() {
        // Arrange: an attacker plants, at the staging path an editor's save
        // would use, a symbolic link pointing at a file outside the bundle.
        let root = temp_root("tmp-symlink");
        std::fs::create_dir_all(root.join("runbooks")).expect("dir");
        let outside = temp_root("tmp-symlink-victim");
        let victim = outside.join("secret.conf");
        std::fs::write(&victim, b"original").expect("victim");
        // The old code used the fixed name `.failover.md.pgokf-tmp`; plant it.
        std::os::unix::fs::symlink(&victim, root.join("runbooks/.failover.md.pgokf-tmp"))
            .expect("plant symlink");

        // Act: write the document whose staging path the link impersonates.
        let wrote = write_confined(&root, "runbooks/failover.md", b"attacker markdown");

        // Assert: the victim file is untouched, whether the write succeeded
        // under a fresh random name or was refused outright.
        assert_eq!(
            std::fs::read(&victim).expect("victim still readable"),
            b"original",
            "a planted temp symlink must never redirect the write"
        );
        if wrote.is_ok() {
            assert_eq!(
                std::fs::read(root.join("runbooks/failover.md")).expect("doc"),
                b"attacker markdown"
            );
        }
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&outside);
    }

    fn pgokf_tmp_files_remain(dir: &Path) -> bool {
        std::fs::read_dir(dir).is_ok_and(|entries| {
            entries.flatten().any(|entry| {
                entry
                    .file_name()
                    .to_str()
                    .is_some_and(|n| n.ends_with(".pgokf-tmp"))
            })
        })
    }
}
