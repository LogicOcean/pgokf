//! Count-only summaries of a sync plan.

use crate::SyncPlan;

/// Count-only summary of a [`SyncPlan`], suitable for user-facing sync results.
///
/// The `PostgreSQL` extension returns this from `register_bundle` / `refresh_bundle`
/// instead of exposing full file listings.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct SyncReport {
    /// Number of newly discovered files.
    pub added: usize,
    /// Number of files whose content changed.
    pub updated: usize,
    /// Number of files that disappeared.
    pub removed: usize,
    /// Number of files whose content is identical.
    pub unchanged: usize,
}

impl SyncReport {
    /// Total number of files the plan accounts for, across all buckets.
    #[must_use]
    pub fn total(self) -> usize {
        self.added + self.updated + self.removed + self.unchanged
    }
}

impl From<&SyncPlan> for SyncReport {
    fn from(plan: &SyncPlan) -> Self {
        Self {
            added: plan.added.len(),
            updated: plan.updated.len(),
            removed: plan.removed.len(),
            unchanged: plan.unchanged.len(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{FileMetadata, UpdatedFile};

    fn metadata(path: &str, contents: &[u8]) -> FileMetadata {
        FileMetadata {
            path: path.into(),
            hash: crate::hash_bytes(contents),
            size_bytes: contents.len() as u64,
            modified_at: None,
        }
    }

    #[test]
    fn report_counts_each_plan_bucket_and_totals_them() {
        let plan = SyncPlan {
            added: vec![metadata("added.md", b"new")],
            updated: vec![UpdatedFile {
                previous: metadata("updated.md", b"old"),
                current: metadata("updated.md", b"new"),
            }],
            removed: vec![metadata("removed.md", b"gone")],
            unchanged: vec![metadata("same.md", b"same"), metadata("also.md", b"same")],
        };

        let report = SyncReport::from(&plan);

        assert_eq!(
            report,
            SyncReport {
                added: 1,
                updated: 1,
                removed: 1,
                unchanged: 2,
            }
        );
        assert_eq!(report.total(), 5);
    }

    #[test]
    fn an_empty_plan_reports_zero_everywhere() {
        let plan = SyncPlan::default();

        let report = SyncReport::from(&plan);

        assert_eq!(report, SyncReport::default());
        assert_eq!(report.total(), 0);
    }
}
