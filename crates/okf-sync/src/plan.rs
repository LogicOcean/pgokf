use std::collections::BTreeMap;
use stratify::logging::tracing;

use crate::FileHash;

/// The catalog-relevant state of one source file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileState {
    pub path: String,
    pub hash: FileHash,
}

impl FileState {
    #[must_use]
    pub fn new(path: impl Into<String>, hash: FileHash) -> Self {
        Self {
            path: path.into(),
            hash,
        }
    }
}

/// Deterministic classification of the difference between two snapshots.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SyncPlan {
    pub added: Vec<String>,
    pub updated: Vec<String>,
    pub removed: Vec<String>,
    pub unchanged: Vec<String>,
}

impl SyncPlan {
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.added.is_empty() && self.updated.is_empty() && self.removed.is_empty()
    }
}

/// Compare previous catalog state with a newly discovered source snapshot.
#[must_use]
pub fn plan_sync(previous: &[FileState], current: &[FileState]) -> SyncPlan {
    let previous = by_path(previous);
    let current = by_path(current);
    let mut plan = SyncPlan::default();

    for (path, hash) in &current {
        match previous.get(path) {
            None => plan.added.push(path.clone()),
            Some(old_hash) if old_hash != hash => plan.updated.push(path.clone()),
            Some(_) => plan.unchanged.push(path.clone()),
        }
    }
    for path in previous.keys() {
        if !current.contains_key(path) {
            plan.removed.push(path.clone());
        }
    }

    tracing::debug!(
        added = plan.added.len(),
        updated = plan.updated.len(),
        removed = plan.removed.len(),
        unchanged = plan.unchanged.len(),
        "planned OKF sync"
    );
    plan
}

fn by_path(states: &[FileState]) -> BTreeMap<String, FileHash> {
    states
        .iter()
        .map(|state| (state.path.clone(), state.hash.clone()))
        .collect()
}
