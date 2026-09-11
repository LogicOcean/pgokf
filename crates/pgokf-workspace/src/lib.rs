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
//!
//! [`build_in_transaction`] runs the whole build - ranking, resolution, seed
//! closure, source reads, freshness reads, and the lock snapshot - under one
//! `REPEATABLE READ` transaction, so the tree can never mix catalog
//! generations. The snapshot stays writable because the audited source
//! readers append one audit row per read inside it. It also applies the
//! selection's stale policy and seed closure; [`build`] is the same flow
//! without the transaction and is kept for callers that cannot hand over a
//! mutable client.

mod archive;
mod freshness;
mod plugin;
mod profile;
mod selection;

use anyhow::{Context, Result};
use tokio_postgres::GenericClient;

pub use archive::{write_to_dir, zip};
pub use freshness::{BundleCheck, PluginCheck, PluginStatus, check_plugin_freshness};
pub use plugin::{
    BuildOptions, Component, LOCK_FILE, MANIFEST_FILE, MCP_SERVER_NAME, Plugin, PluginFile,
    assemble, assemble_with_report, slug,
};
pub use profile::{
    AGENT_PLUGIN_MCP_SCHEMA, AGENT_PLUGIN_SCHEMA, CUSTOM_TARGET_ID, CustomHarness, EnvRef,
    McpFormat, McpSpec, Profile, RemoteSpec, Shape, TOKEN_ENV, Target, TokenRef,
};
pub use selection::{
    BuildRefusal, BuildReport, BundleState, ClosureNode, ClosureReport, ConceptRecord, ConceptRef,
    DEFAULT_HOPS, DEFAULT_LIMIT, Direction, Freshness, MAX_CONCEPTS, MAX_HOPS, Origin,
    PackageRecord, ResourceFile, ResourceRecord, Selection, Snapshot, StaleConcept, StalePolicy,
    UnresolvedEdge, drop_packaged_resources, load_sources, resolve, snapshot,
};

/// Resolve a selection, load its content, and assemble the tree for a
/// target, all on the caller's client. With a plain client each stage is its
/// own statement; [`build_in_transaction`] is the one-snapshot form every
/// in-repo caller uses.
///
/// # Errors
///
/// An empty or unmatched selection, a [`BuildRefusal`], or a catalog failure.
pub async fn build<C: GenericClient>(
    client: &C,
    options: &BuildOptions,
    selection: &Selection,
) -> Result<Plugin> {
    build_scoped(client, options, selection).await
}

/// The build flow on one already-scoped client (a transaction, or a plain
/// connection): resolve with the closure and the stale policy, load sources,
/// snapshot the catalog, assemble.
///
/// # Errors
///
/// An empty or unmatched selection, a [`BuildRefusal`], or a catalog failure.
pub async fn build_scoped<C: GenericClient>(
    client: &C,
    options: &BuildOptions,
    selection: &Selection,
) -> Result<Plugin> {
    let (mut records, report) = selection::resolve_selected(client, selection).await?;
    load_sources(client, &mut records).await?;
    let snapshot = snapshot_for(client, selection, &records, &report).await?;
    assemble_with_report(options, selection, &snapshot, &records, &report)
}

/// The one-snapshot build: open `REPEATABLE READ` on the caller's
/// connection, run the whole build inside it, and commit only after the
/// source reads and the lock snapshot complete. On any failure the
/// transaction rolls back and the error propagates.
///
/// # Errors
///
/// An empty or unmatched selection, a [`BuildRefusal`], or a catalog
/// failure (including opening or closing the snapshot).
pub async fn build_in_transaction(
    client: &mut tokio_postgres::Client,
    options: &BuildOptions,
    selection: &Selection,
) -> Result<Plugin> {
    let transaction = start_snapshot(client).await?;
    match build_scoped(&transaction, options, selection).await {
        Ok(plugin) => {
            transaction
                .commit()
                .await
                .context("closing the catalog snapshot")?;
            Ok(plugin)
        }
        Err(error) => {
            transaction.rollback().await.ok();
            Err(error)
        }
    }
}

/// The resolution half of a build under one snapshot - selection, closure,
/// freshness, and stale policy, plus the catalog snapshot - without reading
/// any source bytes. The web preview uses it so the tree it shows applies
/// exactly the policy the download will.
///
/// # Errors
///
/// An empty or unmatched selection, a [`BuildRefusal`], or a catalog failure.
pub async fn resolve_in_transaction(
    client: &mut tokio_postgres::Client,
    selection: &Selection,
) -> Result<(Vec<ConceptRecord>, Snapshot, BuildReport)> {
    let transaction = start_snapshot(client).await?;
    let outcome = async {
        let (records, report) = selection::resolve_selected(&transaction, selection).await?;
        let snapshot = snapshot_for(&transaction, selection, &records, &report).await?;
        Ok((records, snapshot, report))
    }
    .await;
    match outcome {
        Ok(done) => {
            transaction
                .commit()
                .await
                .context("closing the catalog snapshot")?;
            Ok(done)
        }
        Err(error) => {
            transaction.rollback().await.ok();
            Err(error)
        }
    }
}

/// The lock snapshot, extended with catalog generations and bundle freshness
/// when the build engaged the freshness surface and the catalog has it.
async fn snapshot_for<C: GenericClient>(
    client: &C,
    selection: &Selection,
    records: &[ConceptRecord],
    report: &BuildReport,
) -> Result<Snapshot> {
    let bundle_ids: Vec<i64> = {
        let mut ids: Vec<i64> = records.iter().map(|r| r.bundle_id).collect();
        ids.sort_unstable();
        ids.dedup();
        ids
    };
    let extended = selection::engaged(selection, records)
        && report.capabilities.freshness
        && report.capabilities.catalog_generation;
    selection::snapshot_extended(client, &bundle_ids, extended).await
}

/// The transaction options every build snapshot opens with, as data, so the
/// choice is testable without a database.
#[derive(Debug, Clone, Copy)]
struct SnapshotOptions {
    isolation: tokio_postgres::IsolationLevel,
    read_only: bool,
}

/// The snapshot a build runs in: `REPEATABLE READ`, so the tree never mixes
/// catalog generations, and *not* read-only. The audited content readers
/// (`get_concept_source`, the package readers) append one audit row per read
/// inside the same transaction, and a read-only transaction refuses those
/// writes (SQLSTATE 25006) - read-only mode made every stored-source build
/// fail before materialization. The snapshot's isolation is what the
/// one-generation promise needs; the audit writes are required and must
/// commit with the build.
fn snapshot_options() -> SnapshotOptions {
    SnapshotOptions {
        isolation: tokio_postgres::IsolationLevel::RepeatableRead,
        read_only: false,
    }
}

/// Open the repeatable-read transaction a build runs in.
async fn start_snapshot(
    client: &mut tokio_postgres::Client,
) -> Result<tokio_postgres::Transaction<'_>> {
    let options = snapshot_options();
    client
        .build_transaction()
        .isolation_level(options.isolation)
        .read_only(options.read_only)
        .start()
        .await
        .context("starting the catalog snapshot")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_build_snapshot_is_repeatable_read_but_writable() {
        // The audited source readers write one audit row per read inside the
        // build's transaction; a read-only snapshot fails them with
        // SQLSTATE 25006. Only the isolation level may be asserted here -
        // the audit behaviour itself is PostgreSQL-side.
        let options = snapshot_options();
        assert!(
            matches!(
                options.isolation,
                tokio_postgres::IsolationLevel::RepeatableRead
            ),
            "the one-generation promise needs a repeatable-read snapshot"
        );
        assert!(
            !options.read_only,
            "audited content reads write inside the snapshot"
        );
    }
}
