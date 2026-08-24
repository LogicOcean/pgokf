//! Filesystem discovery, content hashing, and incremental sync planning.

pub mod discover;
pub mod error;
pub mod hash;
pub mod plan;
pub mod report;

pub use discover::{DiscoverOptions, DiscoveredFile, discover};
pub use error::{Error, Result};
pub use hash::{FileHash, hash_bytes, hash_file};
pub use plan::{FileState, SyncPlan, plan_sync};
pub use report::SyncReport;
