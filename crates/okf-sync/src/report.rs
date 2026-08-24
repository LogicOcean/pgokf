use crate::SyncPlan;

/// Count-only summary suitable for user-facing sync results.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SyncReport {
    pub added: usize,
    pub updated: usize,
    pub removed: usize,
    pub unchanged: usize,
}

impl SyncReport {
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
