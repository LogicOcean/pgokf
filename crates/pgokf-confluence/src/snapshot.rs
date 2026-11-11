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

/// Persist a checkpoint atomically after its database transaction committed.
///
/// # Errors
/// Returns an error on filesystem or serialization failure; leaves the old file intact until rename.
pub fn write_checkpoint(path: &Path, snapshot: &Snapshot) -> Result<()> {
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
        #[cfg(unix)]
        File::open(parent)?.sync_all()?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result.context(
        "writing checkpoint after database commit; rerun with the same source to reconcile",
    )
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
                cloud_id: None,
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

    #[test]
    fn malformed_checkpoint_does_not_become_first_run() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        fs::write(&path, b"truncated").unwrap();
        assert!(read_checkpoint(&path).is_err());
    }
}
