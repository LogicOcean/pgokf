// SPDX-License-Identifier: AGPL-3.0-only
//! Snapshot identity, deletion review, and durable local checkpoints.
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};

use crate::types::{Document, SourceIdentity};

/// One successfully converted page, keyed by its stable OKF path.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct Entry {
    pub page_id: String,
    pub version: u64,
    pub hash: String,
}

/// Credential-free binding and manifest of a successfully committed import.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct Snapshot {
    pub schema_version: u32,
    pub source: SourceIdentity,
    pub bundle_name: String,
    pub tenant: String,
    pub database_fingerprint: Option<String>,
    pub bundle_id: Option<i64>,
    pub documents: BTreeMap<String, Entry>,
}

impl Snapshot {
    /// Build a nonempty, duplicate-free snapshot from converted documents.
    ///
    /// # Errors
    /// Rejects unsafe paths, duplicate IDs, empty content, and invalid versions.
    pub fn new(
        source: SourceIdentity,
        bundle_name: String,
        tenant: String,
        docs: &[Document],
    ) -> Result<Self> {
        ensure!(
            !bundle_name.trim().is_empty() && !bundle_name.contains('\0'),
            "bundle name must be nonempty and contain no NUL"
        );
        ensure!(
            !tenant.trim().is_empty() && !tenant.contains('\0'),
            "tenant must be nonempty and contain no NUL"
        );
        ensure!(!docs.is_empty(), "refusing an empty Confluence snapshot");
        let mut documents = BTreeMap::new();
        let mut ids = BTreeSet::new();
        for doc in docs {
            ensure!(
                !doc.page_id.is_empty() && doc.page_id.bytes().all(|c| c.is_ascii_digit()),
                "invalid page ID in converted document"
            );
            ensure!(
                doc.path == format!("pages/{}.md", doc.page_id),
                "unexpected document path"
            );
            ensure!(
                doc.version > 0 && !doc.bytes.is_empty(),
                "empty or unversioned document"
            );
            ensure!(
                ids.insert(&doc.page_id),
                "duplicate page ID in converted snapshot"
            );
            documents.insert(
                doc.path.clone(),
                Entry {
                    page_id: doc.page_id.clone(),
                    version: doc.version,
                    hash: blake3::hash(&doc.bytes).to_hex().to_string(),
                },
            );
        }
        Ok(Self {
            schema_version: 1,
            source,
            bundle_name,
            tenant,
            database_fingerprint: None,
            bundle_id: None,
            documents,
        })
    }

    /// Validate a prior checkpoint belongs to this source and destination.
    ///
    /// # Errors
    /// Returns an error on mismatched identity or an invalid checkpoint.
    pub fn check_previous(&self, previous: &Self) -> Result<()> {
        ensure!(
            previous.schema_version == 1 && !previous.documents.is_empty(),
            "invalid checkpoint schema or empty manifest"
        );
        ensure!(
            self.source == previous.source
                && self.bundle_name == previous.bundle_name
                && self.tenant == previous.tenant,
            "checkpoint belongs to a different source, bundle, or tenant"
        );
        if let Some(fingerprint) = &self.database_fingerprint {
            ensure!(
                previous.database_fingerprint.as_ref() == Some(fingerprint),
                "checkpoint belongs to a different database endpoint"
            );
        }
        for (path, entry) in &previous.documents {
            if let Some(current) = self.documents.get(path) {
                ensure!(
                    current.version >= entry.version,
                    "page version decreased since checkpoint; refusing an older source snapshot"
                );
            }
            ensure!(
                !entry.page_id.is_empty()
                    && entry.page_id.bytes().all(|c| c.is_ascii_digit())
                    && *path == format!("pages/{}.md", entry.page_id)
                    && entry.version > 0
                    && entry.hash.len() == 64
                    && entry.hash.bytes().all(|c| c.is_ascii_hexdigit()),
                "invalid checkpoint document entry"
            );
        }
        Ok(())
    }

    /// Page IDs removed since the last successful import.
    #[must_use]
    pub fn removed_ids(&self, previous: Option<&Self>) -> Vec<String> {
        previous
            .into_iter()
            .flat_map(|state| &state.documents)
            .filter(|(path, _)| !self.documents.contains_key(*path))
            .map(|(_, entry)| entry.page_id.clone())
            .collect()
    }

    /// Hash projection used to compare the exact catalog contents.
    #[must_use]
    pub fn hashes(&self) -> BTreeMap<String, String> {
        self.documents
            .iter()
            .map(|(path, entry)| (path.clone(), entry.hash.clone()))
            .collect()
    }

    /// BLAKE3 over the exact document set (`path` and content hash of every
    /// document, in path order): equal exactly when the submitted bundle is.
    #[must_use]
    pub fn manifest_hash(&self) -> String {
        self.digest(MANIFEST_DOMAIN, |path, entry| {
            format!("{path}\0{}", entry.hash)
        })
    }

    /// BLAKE3 over the Confluence revisions the snapshot materializes (`page
    /// ID` and version of every page, in path order): the producer's opaque
    /// source generation, which moves whenever any page is edited, added or
    /// removed - even when the converted Markdown happens not to change.
    #[must_use]
    pub fn source_generation(&self) -> String {
        self.digest(GENERATION_DOMAIN, |_, entry| {
            format!("{}\0{}", entry.page_id, entry.version)
        })
    }

    /// Domain-separated, newline-framed digest of one record per document.
    /// Paths and page IDs are validated (`pages/<digits>.md`, digits) and
    /// content hashes are hex, so no record can contain the NUL or newline
    /// separators.
    fn digest(&self, domain: &str, record: impl Fn(&str, &Entry) -> String) -> String {
        let mut hasher = blake3::Hasher::new();
        hasher.update(domain.as_bytes());
        for (path, entry) in &self.documents {
            hasher.update(b"\n");
            hasher.update(record(path, entry).as_bytes());
        }
        hasher.finalize().to_hex().to_string()
    }
}

/// Upper bound for a snapshot byte budget. The whole bundle travels to the
/// database in one protocol message, which the server caps at 1 GiB, so a
/// larger snapshot could never be registered; the margin covers the paths,
/// the other parameters and the message framing.
pub const MAX_SNAPSHOT_BYTES_LIMIT: usize = 960 * 1024 * 1024;

/// Total bytes a snapshot submits (document bytes plus paths), refused above
/// `max_bytes`. Checked in a preview too, so a dry run never reports a
/// snapshot that `--apply` could not register.
///
/// # Errors
/// Returns an error when the total exceeds `max_bytes`.
pub fn ensure_snapshot_size(docs: &[Document], max_bytes: usize) -> Result<usize> {
    let total = docs
        .iter()
        .try_fold(0_usize, |sum, doc| {
            sum.checked_add(doc.bytes.len())?
                .checked_add(doc.path.len())
        })
        .unwrap_or(usize::MAX);
    ensure!(
        total <= max_bytes,
        "converted space is {total} bytes, above --max-snapshot-bytes ({max_bytes}); \
         one import must fit a single PostgreSQL message"
    );
    Ok(total)
}

/// Domain separators: a manifest hash can never equal a source generation.
const MANIFEST_DOMAIN: &str = "pgokf-confluence.manifest.v1";
const GENERATION_DOMAIN: &str = "pgokf-confluence.source-generation.v1";

/// A separate persistent lock file; atomic checkpoint replacement keeps the lock held.
pub struct StateLock {
    _file: File,
}

impl StateLock {
    /// Acquire an exclusive, nonblocking checkpoint lock for the entire run.
    ///
    /// # Errors
    /// Fails on overlapping runs or filesystem errors.
    pub fn acquire(state_path: &Path) -> Result<Self> {
        let mut lock_name = state_path.as_os_str().to_os_string();
        lock_name.push(".lock");
        let lock_path = PathBuf::from(lock_name);
        fs::create_dir_all(parent_dir(&lock_path))?;
        let file = private_options()
            .create(true)
            .truncate(false)
            .open(lock_path)
            .context("opening checkpoint lock")?;
        file.try_lock().context(
            "checkpoint is locked by another import, or filesystem locking is unavailable",
        )?;
        Ok(Self { _file: file })
    }
}

fn private_options() -> OpenOptions {
    let mut options = OpenOptions::new();
    options.write(true).read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options
}

fn parent_dir(path: &Path) -> &Path {
    path.parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
}

/// Read a prior checkpoint, treating only a missing file as a first run.
///
/// # Errors
/// Returns an error on malformed or unreadable checkpoints.
pub fn read_checkpoint(path: &Path) -> Result<Option<Snapshot>> {
    match fs::read(path) {
        Ok(bytes) => Ok(Some(
            serde_json::from_slice(&bytes).context("invalid checkpoint JSON")?,
        )),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).context("reading checkpoint"),
    }
}

/// Persist a checkpoint atomically.
///
/// # Errors
/// Returns an error on filesystem or serialization failure; leaves the old file intact until rename.
pub fn write_checkpoint(path: &Path, snapshot: &Snapshot) -> Result<()> {
    write_atomically(path, snapshot).context("writing checkpoint")
}

/// The pending checkpoint beside a checkpoint: the snapshot of an import
/// whose transaction is about to commit, or committed without the
/// checkpoint being promoted (a crash, a full disk, a lost commit reply).
#[must_use]
pub fn pending_path(state_path: &Path) -> PathBuf {
    let mut name = state_path.as_os_str().to_os_string();
    name.push(".pending");
    PathBuf::from(name)
}

/// Read a pending checkpoint; absent means the last import was promoted.
///
/// # Errors
/// Returns an error on a malformed or unreadable pending checkpoint.
pub fn read_pending(state_path: &Path) -> Result<Option<Snapshot>> {
    read_checkpoint(&pending_path(state_path)).context("reading pending checkpoint")
}

/// Durably record the snapshot an import is about to commit. Written inside
/// the transaction, before COMMIT, so whatever happens to the commit or the
/// process afterwards, the next run can tell which snapshot the catalog holds.
///
/// # Errors
/// Returns an error on filesystem or serialization failure.
pub fn write_pending(state_path: &Path, snapshot: &Snapshot) -> Result<()> {
    write_atomically(&pending_path(state_path), snapshot).context("writing pending checkpoint")
}

/// Make a committed pending checkpoint the checkpoint, atomically.
///
/// # Errors
/// Returns an error when the rename or the directory sync fails.
pub fn promote_pending(state_path: &Path) -> Result<()> {
    fs::rename(pending_path(state_path), state_path)
        .and_then(|()| sync_parent(state_path))
        .context("promoting the pending checkpoint")
}

/// Drop a pending checkpoint whose import did not commit.
///
/// # Errors
/// Returns an error when the file exists but cannot be removed.
pub fn discard_pending(state_path: &Path) -> Result<()> {
    match fs::remove_file(pending_path(state_path)) {
        Ok(()) => sync_parent(state_path).context("discarding the pending checkpoint"),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).context("discarding the pending checkpoint"),
    }
}

/// Write `snapshot` to `path` through a synced temporary file and a rename,
/// so a reader sees the old file or the complete new one, never a torn one.
fn write_atomically(path: &Path, snapshot: &Snapshot) -> Result<()> {
    let parent = parent_dir(path);
    fs::create_dir_all(parent)?;
    let tmp = parent.join(format!(
        ".pgokf-confluence-{:016x}.tmp",
        rand::random::<u64>()
    ));
    let result = (|| -> Result<()> {
        let mut file = private_options().create_new(true).open(&tmp)?;
        file.write_all(&serde_json::to_vec_pretty(snapshot)?)?;
        file.write_all(b"\n")?;
        file.sync_all()?;
        fs::rename(&tmp, path)?;
        sync_parent(path)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

/// Persist a rename or removal in `path`'s directory.
fn sync_parent(path: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    File::open(parent_dir(path))?.sync_all()?;
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

/// Export a complete snapshot to a new directory, never mixing in old files.
///
/// # Errors
/// Rejects existing output directories and any filesystem failure.
pub fn export(path: &Path, snapshot: &Snapshot, docs: &[Document]) -> Result<()> {
    ensure!(
        !path.exists(),
        "output directory already exists; choose a new directory for each export"
    );
    fs::create_dir_all(parent_dir(path))?;
    fs::create_dir(path).context("creating fresh export directory")?;
    let result = (|| -> Result<()> {
        fs::create_dir(path.join("pages"))?;
        for doc in docs {
            ensure!(
                snapshot.documents.contains_key(&doc.path),
                "export document missing from snapshot"
            );
            let mut file = private_options()
                .create_new(true)
                .open(path.join(&doc.path))?;
            file.write_all(&doc.bytes)?;
        }
        let mut manifest = private_options()
            .create_new(true)
            .open(path.join("manifest.json"))?;
        manifest.write_all(&serde_json::to_vec_pretty(snapshot)?)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_dir_all(path);
    }
    result.context("exporting Confluence snapshot")
}

#[cfg(test)]
mod tests {
    use super::*;

    pub fn snapshot(ids: &[&str]) -> Snapshot {
        let docs: Vec<_> = ids
            .iter()
            .map(|id| Document {
                path: format!("pages/{id}.md"),
                bytes: format!("page {id}").into_bytes(),
                page_id: (*id).into(),
                version: 1,
                warnings: vec![],
            })
            .collect();
        Snapshot::new(
            SourceIdentity {
                site_url: "https://test.atlassian.net".into(),
                space_id: "42".into(),
            },
            "wiki".into(),
            "default".into(),
            &docs,
        )
        .unwrap()
    }

    #[test]
    fn deletions_are_exact_and_source_binding_is_required() {
        let old = snapshot(&["1", "2", "3"]);
        let mut new = snapshot(&["1", "3"]);
        assert_eq!(new.removed_ids(Some(&old)), vec!["2"]);
        new.check_previous(&old).unwrap();
        new.source.space_id = "99".into();
        assert!(new.check_previous(&old).is_err());
    }

    #[test]
    fn stale_source_versions_cannot_replace_newer_checkpoints() {
        let mut old = snapshot(&["1"]);
        old.documents.get_mut("pages/1.md").unwrap().version = 2;
        let new = snapshot(&["1"]);
        assert!(new.check_previous(&old).is_err());
    }

    #[test]
    fn empty_and_duplicate_snapshots_are_rejected() {
        let old = snapshot(&["1"]);
        assert!(Snapshot::new(old.source.clone(), "wiki".into(), "default".into(), &[]).is_err());
        let doc = Document {
            path: "pages/1.md".into(),
            bytes: vec![1],
            page_id: "1".into(),
            version: 1,
            warnings: vec![],
        };
        assert!(
            Snapshot::new(
                old.source,
                "wiki".into(),
                "default".into(),
                &[doc.clone(), doc]
            )
            .is_err()
        );
    }

    #[test]
    fn state_lock_survives_atomic_replacement() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        let lock = StateLock::acquire(&path).unwrap();
        assert!(read_checkpoint(&path).unwrap().is_none());
        let state = snapshot(&["1"]);
        write_checkpoint(&path, &state).unwrap();
        assert_eq!(read_checkpoint(&path).unwrap(), Some(state));
        assert!(StateLock::acquire(&path).is_err());
        drop(lock);
        assert!(StateLock::acquire(&path).is_ok());
    }

    #[test]
    fn manifest_hash_is_stable_for_the_same_document_set() {
        // Arrange
        let first = snapshot(&["1", "2"]);
        let second = snapshot(&["1", "2"]);

        // Act / Assert
        assert_eq!(first.manifest_hash(), second.manifest_hash());
        assert_eq!(first.manifest_hash().len(), 64);
    }

    #[test]
    fn manifest_hash_changes_when_content_changes() {
        // Arrange
        let before = snapshot(&["1"]);
        let mut after = before.clone();
        after.documents.get_mut("pages/1.md").unwrap().hash = "f".repeat(64);

        // Act / Assert
        assert_ne!(before.manifest_hash(), after.manifest_hash());
    }

    #[test]
    fn source_generation_moves_with_a_version_even_when_content_is_identical() {
        // Arrange
        let before = snapshot(&["1"]);
        let mut after = before.clone();
        after.documents.get_mut("pages/1.md").unwrap().version = 2;

        // Act / Assert
        assert_eq!(before.manifest_hash(), after.manifest_hash());
        assert_ne!(before.source_generation(), after.source_generation());
    }

    #[test]
    fn manifest_hash_and_source_generation_are_domain_separated() {
        // Arrange
        let state = snapshot(&["1"]);

        // Act / Assert
        assert_ne!(state.manifest_hash(), state.source_generation());
    }

    fn document(id: &str, size: usize) -> Document {
        Document {
            path: format!("pages/{id}.md"),
            bytes: vec![b'x'; size],
            page_id: id.into(),
            version: 1,
            warnings: vec![],
        }
    }

    #[test]
    fn ensure_snapshot_size_counts_bytes_and_paths() {
        // Arrange
        let docs = [document("1", 100), document("22", 50)];

        // Act
        let total = ensure_snapshot_size(&docs, 1_000).unwrap();

        // Assert: 150 bytes of content plus "pages/1.md" and "pages/22.md".
        assert_eq!(total, 150 + 10 + 11);
    }

    #[test]
    fn ensure_snapshot_size_accepts_exactly_the_budget() {
        // Arrange
        let docs = [document("1", 90)];

        // Act / Assert
        assert!(ensure_snapshot_size(&docs, 100).is_ok());
    }

    #[test]
    fn ensure_snapshot_size_refuses_one_byte_over_the_budget() {
        // Arrange
        let docs = [document("1", 91)];

        // Act
        let error = ensure_snapshot_size(&docs, 100).unwrap_err();

        // Assert
        assert!(
            error.to_string().contains("--max-snapshot-bytes"),
            "{error}"
        );
    }

    #[test]
    fn check_previous_refuses_a_checkpoint_from_another_database_endpoint() {
        // Arrange
        let mut previous = snapshot(&["1"]);
        previous.database_fingerprint = Some("a".repeat(64));
        let mut next = snapshot(&["1"]);
        next.database_fingerprint = Some("b".repeat(64));

        // Act
        let result = next.check_previous(&previous);

        // Assert
        assert!(result.is_err());
    }

    #[test]
    fn pending_checkpoint_is_separate_until_promoted() {
        // Arrange
        let dir = tempfile::tempdir().unwrap();
        let state = dir.path().join("state.json");
        let committed = snapshot(&["1"]);
        let committing = snapshot(&["1", "2"]);
        write_checkpoint(&state, &committed).unwrap();

        // Act
        write_pending(&state, &committing).unwrap();

        // Assert
        assert_eq!(read_checkpoint(&state).unwrap(), Some(committed));
        assert_eq!(read_pending(&state).unwrap(), Some(committing));
    }

    #[test]
    fn promote_pending_replaces_the_checkpoint_and_clears_pending() {
        // Arrange
        let dir = tempfile::tempdir().unwrap();
        let state = dir.path().join("state.json");
        let committing = snapshot(&["1", "2"]);
        write_checkpoint(&state, &snapshot(&["1"])).unwrap();
        write_pending(&state, &committing).unwrap();

        // Act
        promote_pending(&state).unwrap();

        // Assert
        assert_eq!(read_checkpoint(&state).unwrap(), Some(committing));
        assert!(read_pending(&state).unwrap().is_none());
    }

    #[test]
    fn discard_pending_keeps_the_checkpoint_and_tolerates_absence() {
        // Arrange
        let dir = tempfile::tempdir().unwrap();
        let state = dir.path().join("state.json");
        let committed = snapshot(&["1"]);
        write_checkpoint(&state, &committed).unwrap();
        write_pending(&state, &snapshot(&["1", "2"])).unwrap();

        // Act
        discard_pending(&state).unwrap();
        let second = discard_pending(&state);

        // Assert
        assert!(read_pending(&state).unwrap().is_none());
        assert_eq!(read_checkpoint(&state).unwrap(), Some(committed));
        assert!(second.is_ok());
    }

    #[test]
    fn promote_pending_fails_without_a_pending_checkpoint() {
        // Arrange
        let dir = tempfile::tempdir().unwrap();
        let state = dir.path().join("state.json");

        // Act / Assert
        assert!(promote_pending(&state).is_err());
    }

    #[test]
    fn malformed_checkpoint_does_not_become_first_run() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        fs::write(&path, b"truncated").unwrap();
        assert!(read_checkpoint(&path).is_err());
    }
}
