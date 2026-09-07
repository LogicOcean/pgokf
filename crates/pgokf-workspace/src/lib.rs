// SPDX-License-Identifier: AGPL-3.0-only
//! Workspace injection for the pgokf catalog (spec §21): build an agent
//! plugin from a catalog selection and deliver it as an Agent Skills
//! package, an `AGENTS.md` instruction file, a prompt bundle, or a generic
//! index-plus-files tree.
//!
//! The crate holds no catalogue semantics and no credentials: selectors
//! resolve through the reader API on a connection the caller supplies, so
//! tenant scope, visibility, retirement, and non-disclosure are enforced by
//! the database. `pgokf-web` (the plugin builder page) and `pgokf-mcp` (the
//! `build_workspace_plugin` tool) are thin fronts over [`build`].
//!
//! A build against an unchanged catalog is byte-identical: no timestamps
//! enter the tree, the lockfile records the catalog snapshot and a content
//! hash per file, and the archive uses fixed entry times.

mod archive;
mod plugin;
mod profile;
mod selection;

use anyhow::Result;
use tokio_postgres::GenericClient;

pub use archive::{write_to_dir, zip};
pub use plugin::{
    BuildOptions, Component, LOCK_FILE, MANIFEST_FILE, MCP_SERVER_NAME, Plugin, PluginFile,
    assemble, slug,
};
pub use profile::{EnvRef, McpFormat, McpSpec, Profile, Shape, Target};
pub use selection::{
    BundleState, ConceptRecord, DEFAULT_LIMIT, MAX_CONCEPTS, PackageRecord, ResourceFile,
    ResourceRecord, Selection, Snapshot, drop_packaged_resources, load_sources, resolve, snapshot,
};

/// Resolve a selection, load its content, and assemble the tree for a target.
///
/// # Errors
///
/// An empty or unmatched selection, or a catalog failure.
pub async fn build<C: GenericClient>(
    client: &C,
    options: &BuildOptions,
    selection: &Selection,
) -> Result<Plugin> {
    let mut records = resolve(client, selection).await?;
    load_sources(client, &mut records).await?;
    let bundle_ids: Vec<i64> = {
        let mut ids: Vec<i64> = records.iter().map(|r| r.bundle_id).collect();
        ids.sort_unstable();
        ids.dedup();
        ids
    };
    let snapshot = snapshot(client, &bundle_ids).await?;
    assemble(options, selection, &snapshot, &records)
}
