//! Incremental sync planning: comparing two snapshots deterministically.

use std::collections::BTreeSet;

use crate::{FileMetadata, Snapshot, SyncConfig, SyncError, discover};

/// A file whose current contents differ from the preceding snapshot.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UpdatedFile {
    /// The file as recorded by the previous snapshot.
    pub previous: FileMetadata,
    /// The file as discovered now.
    pub current: FileMetadata,
}

/// The work required to move a persisted snapshot to a newly discovered snapshot.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct SyncPlan {
    /// Files present now that the previous snapshot did not contain.
    pub added: Vec<FileMetadata>,
    /// Files whose content hash changed since the previous snapshot.
    pub updated: Vec<UpdatedFile>,
    /// Files the previous snapshot contained that no longer exist.
    pub removed: Vec<FileMetadata>,
    /// Files whose content hash is identical in both snapshots.
    pub unchanged: Vec<FileMetadata>,
}

impl SyncPlan {
    /// Whether the plan requires no mutations (unchanged files do not count).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.added.is_empty() && self.updated.is_empty() && self.removed.is_empty()
    }
}

/// Compare `previous` to `current` and construct deterministic incremental work.
///
/// A metadata-only change is unchanged: the BLAKE3 hash is the content identity used
/// to decide whether downstream consumers need to synchronize a document.
#[must_use]
pub fn plan(previous: &Snapshot, current: &Snapshot) -> SyncPlan {
    let paths: BTreeSet<_> = previous
        .files()
        .keys()
        .chain(current.files().keys())
        .collect();
    let mut result = SyncPlan::default();

    for path in paths {
        match (previous.files().get(path), current.files().get(path)) {
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

    tracing::debug!(
        added = result.added.len(),
        updated = result.updated.len(),
        removed = result.removed.len(),
        unchanged = result.unchanged.len(),
        "planned OKF sync"
    );
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

#[cfg(test)]
mod tests {
    use std::{fs, path::PathBuf, thread, time::Duration};

    use tempfile::TempDir;

    use super::*;
    use crate::test_support::write_file;

    #[test]
    fn incremental_plan_classifies_added_updated_removed_and_unchanged() {
        let root = TempDir::new().unwrap();
        write_file(&root, "unchanged.md", "same");
        write_file(&root, "updated.md", "before");
        write_file(&root, "removed.md", "gone");
        let config = SyncConfig::new(root.path());
        let before = discover(&config).unwrap();

        write_file(&root, "updated.md", "after");
        fs::remove_file(root.path().join("removed.md")).unwrap();
        write_file(&root, "added.md", "new");
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
        write_file(&root, "doc.md", "stable");
        let config = SyncConfig::new(root.path());
        let before = discover(&config).unwrap();
        thread::sleep(Duration::from_millis(5));
        write_file(&root, "doc.md", "stable");
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
        write_file(&root, "document.md", "first version");

        let (first_snapshot, first_plan) = build_plan(&config, &empty).unwrap();
        assert_eq!(first_plan.added.len(), 1);

        write_file(&root, "document.md", "second version");
        let (_, second_plan) = build_plan(&config, &first_snapshot).unwrap();
        assert_eq!(second_plan.updated.len(), 1);
        assert_eq!(second_plan.added.len(), 0);
    }
}
