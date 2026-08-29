// SPDX-License-Identifier: AGPL-3.0-only
//! Filesystem-backed incremental synchronization for Open Knowledge Format bundles.
//!
//! This crate deliberately has no database dependency. It discovers Markdown files,
//! snapshots their contents and metadata, then compares snapshots to produce a
//! deterministic [`SyncPlan`]; a [`SyncReport`] condenses the plan into the counts
//! surfaced to end users. Application binaries can initialize configuration and
//! logging with `stratify`; the planner itself is synchronous and side-effect free
//! apart from reading its configured bundle directory.
//!
//! # Safety properties
//!
//! - Symlinks are never followed while walking, and a symlink whose resolved target
//!   escapes the bundle root is rejected with [`SyncError::SymlinkEscape`].
//! - [`SyncConfig::max_file_bytes`] and [`SyncConfig::max_files`] bound the scan;
//!   file sizes are checked from metadata before any contents are read.
//!
//! # Module layout
//!
//! - [`config`](SyncConfig): what to scan and which limits apply
//! - [`discover`]: walking the bundle and snapshotting its documents
//! - [`plan`]/[`build_plan`]: comparing snapshots into incremental work
//! - [`SyncReport`]: count-only summary of a plan
//! - [`hash_bytes`]/[`hash_file`]: BLAKE3 content hashing helpers
//! - [`SyncError`]: actionable failures from all of the above

mod config;
mod discover;
mod error;
mod hash;
mod plan;
mod report;

pub use config::SyncConfig;
pub use discover::{FileMetadata, Snapshot, discover};
pub use error::SyncError;
pub use hash::{hash_bytes, hash_file};
pub use plan::{SyncPlan, UpdatedFile, build_plan, plan};
pub use report::SyncReport;

/// Re-export the service bootstrap used by OKF applications for configuration and
/// logging. Callers should initialize it before invoking [`discover`] when they want
/// the planner's `tracing` diagnostics to be collected.
pub use stratify;

#[cfg(test)]
pub(crate) mod test_support {
    use std::fs;

    use tempfile::TempDir;

    /// Write `contents` to `relative` under the temporary root, creating parents.
    pub(crate) fn write_file(root: &TempDir, relative: &str, contents: &str) {
        let path = root.path().join(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, contents).unwrap();
    }
}
