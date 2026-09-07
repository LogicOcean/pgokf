// SPDX-License-Identifier: AGPL-3.0-only
//! The read-only data layer: a pooled `pgokf_reader` connection and one typed
//! query per catalog surface the UI shows.
//!
//! Every query goes through the public SQL API (`pgokf.*` functions) or the
//! reader-visible projection tables, with every caller value bound as a
//! parameter. Tenant scope, visibility, `require_tenant`, and non-disclosure
//! are enforced by the database on the pooled role; this layer never bypasses
//! them and never holds a writer or admin role.
//!
//! Rows are read through [`col`], which turns an unexpected NULL or type into
//! an error the page can report instead of a panic that drops the connection.

use std::collections::BTreeMap;
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use deadpool_postgres::{
    Hook, HookError, Manager, ManagerConfig, Object, Pool, PoolError, RecyclingMethod, Runtime,
};
use serde::Serialize;
use serde_json::Value;
use tokio_postgres::types::{FromSql, ToSql};
use tokio_postgres::{NoTls, Row};

/// Pool settings.
pub(crate) struct DbConfig<'a> {
    pub database_url: &'a str,
    pub force_tls: bool,
    pub pool_size: usize,
    pub tenant: Option<&'a str>,
    pub statement_timeout_ms: u64,
}

/// How long a request waits for a pooled connection before it is turned
/// away as "busy" rather than queued without bound.
const POOL_WAIT: Duration = Duration::from_secs(5);

/// A pooled reader connection to one catalog.
#[derive(Clone)]
pub(crate) struct Db {
    pool: Pool,
}

/// The name a content bundle is keyed on: its registered name, or the
/// synthetic path `content:<name>` without the prefix.
pub(crate) fn content_bundle_name(path: &str, name: Option<&str>) -> String {
    path.strip_prefix("content:")
        .or(name)
        .unwrap_or(path)
        .to_owned()
}

/// One row of `pgokf.list_bundles()`.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct BundleInfo {
    pub id: i64,
    pub path: String,
    /// The registered label, or the last path segment when none was given.
    pub name: String,
    pub okf_version: Option<String>,
    pub file_count: i32,
    pub last_synced_at: Option<String>,
    pub enabled: bool,
}

/// One row of `pgokf.catalog_stats()`.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct BundleStat {
    pub bundle_id: i64,
    /// The registered label, or the last path segment when none was given.
    pub name: String,
    pub enabled: bool,
    pub source_type: String,
    pub file_count: i32,
    pub indexed_concepts: i64,
    pub link_count: i64,
    pub resolved_link_count: i64,
    pub last_synced_at: Option<String>,
    /// Seconds since the last sync, or `None` when never synced.
    pub sync_age_seconds: Option<i64>,
    pub is_stale: bool,
    pub retired_at: Option<String>,
}

/// One file of a content bundle as the catalog stores it, for the human
/// workflow's full-snapshot resyncs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BundleFile {
    pub path: String,
    pub bytes: Vec<u8>,
}

/// A content bundle the human workflow can write to.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct ContentBundle {
    pub id: i64,
    /// The name `register_bundle_content` keys the bundle on.
    pub name: String,
    pub file_count: i32,
}

/// What `pgokf.register_bundle_content` reports.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SyncOutcome {
    pub bundle_id: i64,
    pub added: i32,
    pub updated: i32,
    pub removed: i32,
}

/// One concept awaiting a human review.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct ReviewItem {
    pub bundle_id: i64,
    pub bundle_name: String,
    pub concept_id: String,
    pub path: String,
    pub title: Option<String>,
    pub concept_type: Option<String>,
    pub status: Option<String>,
    pub trust_tier: String,
    pub generated_by: Option<String>,
    pub modified_at: Option<String>,
}

/// A concept as listed inside a bundle or a filter-only browse.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct ConceptSummary {
    pub bundle_id: i64,
    pub concept_id: String,
    pub path: String,
    pub concept_type: Option<String>,
    pub title: Option<String>,
    pub description: Option<String>,
    pub tags: Vec<String>,
    pub modified_at: Option<String>,
}

/// One search hit (`pgokf.concept_search_result`).
#[derive(Debug, Clone, Serialize)]
pub(crate) struct Hit {
    pub bundle_id: i64,
    pub concept_id: String,
    pub path: String,
    pub title: Option<String>,
    pub concept_type: Option<String>,
    pub rank: f32,
    /// `ts_headline` markup, sanitized to `<b>` only before rendering.
    pub headline: Option<String>,
}

/// One facet bucket.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct Facet {
    pub value: String,
    pub count: i64,
}

/// The filters a search carries; `None` means "no filter" for every field.
#[derive(Debug, Clone, Default)]
pub(crate) struct SearchQuery {
    pub query: String,
    pub bundle_id: Option<i64>,
    pub concept_type: Option<String>,
    pub tags: Vec<String>,
    pub status: Option<String>,
    pub trust_tier: Option<String>,
    pub limit: i32,
    pub after: Option<Cursor>,
}

impl SearchQuery {
    /// `true` when any filter narrows the result set.
    pub(crate) fn has_filters(&self) -> bool {
        self.bundle_id.is_some()
            || self.concept_type.is_some()
            || !self.tags.is_empty()
            || self.status.is_some()
            || self.trust_tier.is_some()
    }

    fn tags_param(&self) -> Option<Vec<String>> {
        (!self.tags.is_empty()).then(|| self.tags.clone())
    }
}

/// A keyset cursor copied from the previous page's last row.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct Cursor {
    pub rank: f32,
    pub bundle_id: i64,
    pub concept_id: String,
}

/// Everything the concept page shows about one concept.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct ConceptDetail {
    pub bundle_id: i64,
    pub bundle_name: String,
    pub concept_id: String,
    pub path: String,
    pub concept_type: Option<String>,
    pub title: Option<String>,
    pub description: Option<String>,
    pub tags: Vec<String>,
    pub resource: Option<String>,
    pub body_text: String,
    pub file_hash: String,
    pub modified_at: Option<String>,
    pub indexed_at: String,
    pub tenant_id: String,
    pub metadata: BTreeMap<String, Value>,
    pub provenance: Option<Provenance>,
    pub verifications: Vec<Verification>,
    pub sources: Vec<ProvenanceSource>,
    /// The stored source bytes when the catalog keeps them (`store_source`).
    /// Read from the projection table, so rendering a page is not an audited
    /// download; the download route goes through `get_concept_source`.
    #[serde(skip)]
    pub source: Option<Vec<u8>>,
    pub has_source: bool,
}

/// `pgokf.concept_provenance` for one concept.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct Provenance {
    pub generated_by: Option<String>,
    pub generated_at: Option<String>,
    pub status: Option<String>,
    pub stale_after: Option<String>,
    pub usage_window_from: Option<String>,
    pub usage_window_to: Option<String>,
    pub trust_tier: Option<String>,
    pub details: Value,
}

/// One `verified` event.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct Verification {
    pub ordinal: i32,
    pub verified_by: String,
    pub verified_at: Option<String>,
}

/// One `sources` entry.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct ProvenanceSource {
    pub ordinal: i32,
    pub source_id: Option<String>,
    pub resource: Option<String>,
    pub title: Option<String>,
    pub author: Option<String>,
    pub usage_count: Option<i64>,
    pub last_modified: Option<String>,
}

/// One concept as the builder's file picker lists it.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct TreeEntry {
    pub id: String,
    pub path: String,
    #[serde(rename = "type")]
    pub concept_type: Option<String>,
    pub title: Option<String>,
    /// A skill manifest: picking it copies the whole package.
    pub package: bool,
    /// The owning skill's id when the concept is a package member.
    pub package_of: Option<String>,
    pub tags: Vec<String>,
}

/// A skill package (`pgokf.skills`) with the resources it owns.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct PackageInfo {
    /// The Agent Skills `name`.
    pub name: String,
    /// Bundle-relative package directory (`""` for a root package).
    pub root: String,
    pub hash: String,
    pub visibility: String,
    pub resources: Vec<ResourceInfo>,
}

/// One package resource: a row of `pgokf.scripts` or
/// `pgokf.reference_documents`.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct ResourceInfo {
    pub concept_id: String,
    /// `script`, `reference`, or `asset`.
    pub class: String,
    /// Package-relative path.
    pub path: String,
    pub byte_size: i64,
    pub sha256: String,
    /// The script's language, or the reference's media type.
    pub detail: String,
    pub package_concept_id: String,
    /// Whether the catalog holds the bytes as text (a script, or a textual
    /// reference); a binary asset has no readable body.
    pub textual: bool,
    /// The exact stored text of a textual reference (`text_body`), so the
    /// page can render the document rather than its search text. `None` for
    /// a script (its body text is already exact) or a binary.
    #[serde(skip)]
    pub text: Option<String>,
}

impl ResourceInfo {
    /// Whether the resource is a Markdown document, rendered like a concept
    /// body; anything else textual is shown verbatim.
    pub(crate) fn is_markdown(&self) -> bool {
        self.detail == "text/markdown"
    }
}

/// A package file's exact bytes for a download.
pub(crate) struct ExactBytes {
    pub bytes: Vec<u8>,
    pub media_type: String,
    /// The file is a `SKILL.md` (named so on download; a resource keeps its
    /// own name).
    pub is_manifest: bool,
}

/// One edge of the link graph, as stored.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct Link {
    pub bundle_id: i64,
    pub source_id: String,
    pub target_id: Option<String>,
    pub text: Option<String>,
    /// The normalized bundle-relative target path (`NULL` for external links).
    pub target_path: Option<String>,
    pub kind: String,
    pub relation: String,
    pub resolved: bool,
    pub is_external: bool,
    /// Title of the concept at the other end of the edge (the target for an
    /// outgoing link, the source for an incoming one) when it resolves.
    pub counterpart_title: Option<String>,
}

/// One row of `pgokf.concept_neighbors()`.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct Neighbor {
    pub bundle_id: i64,
    pub id: String,
    pub title: Option<String>,
    pub hops: i32,
    /// The chain of concept ids from the seed to this neighbor.
    pub path: Vec<String>,
}

/// A node of a graph picture: a concept with what the picture labels it by.
/// `hops` is the distance from the seed in a neighborhood graph and 0 in the
/// catalog-wide graph; `degree` is the number of resolved links it takes
/// part in.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct GraphNode {
    pub bundle_id: i64,
    pub bundle_name: String,
    pub id: String,
    pub title: Option<String>,
    pub concept_type: Option<String>,
    pub path: String,
    pub hops: i32,
    pub degree: i64,
}

/// A directed edge between two graph nodes of one bundle, folded over
/// parallel links, keeping the distinct link texts (a few) for inspection.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct GraphLink {
    pub bundle_id: i64,
    pub source: String,
    pub target: String,
    pub count: i64,
    pub relations: Vec<String>,
    pub texts: Vec<String>,
}

/// A graph picture as nodes and edges.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct Graph {
    pub nodes: Vec<GraphNode>,
    pub links: Vec<GraphLink>,
    /// Visible concepts that could have been drawn (the catalog-wide graph
    /// shows the best-connected `limit` of them).
    pub total: i64,
}

/// How many distinct link texts an edge carries into the picture.
const EDGE_TEXTS: usize = 5;

/// One row of `pgokf.concept_history()`.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct Version {
    pub number: i64,
    pub valid_from: String,
    pub valid_to: Option<String>,
    pub change_kind: String,
    pub concept_type: Option<String>,
    pub title: Option<String>,
    /// `None` for a removal tombstone.
    pub file_hash: Option<String>,
}

/// One row of `pgokf.list_sync_log()`. The bundle and the counters are
/// `None` for an `unregister` row, which outlives its bundle.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct SyncLogEntry {
    pub id: i64,
    pub bundle_id: Option<i64>,
    pub bundle_path: Option<String>,
    pub op: String,
    pub actor: String,
    pub synced_at: String,
    pub added: Option<i32>,
    pub updated: Option<i32>,
    pub removed: Option<i32>,
    pub unchanged: Option<i32>,
    pub total: Option<i32>,
}

/// One row of `pgokf.list_bundle_log()`.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct BundleLogEntry {
    pub directory: String,
    pub ordinal: i32,
    pub logged_at: Option<String>,
    pub entry: String,
}

/// One row of `pgokf.stale_concepts()`, with the concept's title joined in.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct StaleConcept {
    pub bundle_id: i64,
    pub concept_id: String,
    pub path: String,
    pub title: Option<String>,
    pub stale_after: Option<String>,
}

/// One row of `pgokf.duplicate_concepts()`.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct DuplicateGroup {
    pub file_hash: String,
    pub occurrences: i64,
    pub bundle_ids: Vec<i64>,
    pub concept_ids: Vec<String>,
}

/// What a failed query means to the caller, derived from the error chain.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Failure {
    /// No pooled connection became free in time.
    Busy,
    /// The database rejected a caller value (SQLSTATE class 22).
    InvalidInput,
    /// A statement hit the configured timeout.
    Timeout,
    /// Anything else.
    Other,
}

/// Classify an error from this module for the HTTP layer.
pub(crate) fn classify(error: &anyhow::Error) -> Failure {
    for cause in error.chain() {
        if let Some(PoolError::Timeout(_)) = cause.downcast_ref::<PoolError>() {
            return Failure::Busy;
        }
        if let Some(pg) = cause.downcast_ref::<tokio_postgres::Error>()
            && let Some(db) = pg.as_db_error()
        {
            let code = db.code().code();
            return match code {
                "57014" => Failure::Timeout,
                _ if code.starts_with("22") => Failure::InvalidInput,
                _ => Failure::Other,
            };
        }
    }
    Failure::Other
}

/// The database's own message for an invalid-input failure, when there is
/// one; these are written for callers (`limit_count must be ...`).
pub(crate) fn db_message(error: &anyhow::Error) -> Option<String> {
    error
        .chain()
        .filter_map(|cause| cause.downcast_ref::<tokio_postgres::Error>())
        .filter_map(tokio_postgres::Error::as_db_error)
        .map(|db| db.message().to_owned())
        .next()
}

/// ISO-8601 rendering of a timestamptz, done in SQL so no date crate is
/// needed on this side.
const ISO: &str = "to_char($COL AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"')";

fn iso(column: &str) -> String {
    ISO.replace("$COL", column)
}

/// A bundle's display name: the registered label, else the last path
/// segment. `register_bundle` leaves `name` NULL when none is given.
const DISPLAY_NAME: &str = "coalesce($T.name, regexp_replace($T.path, '^.*/', ''))";

fn display_name(table: &str) -> String {
    DISPLAY_NAME.replace("$T", table)
}

/// Read one column, turning a NULL or type mismatch into an error that
/// names the column instead of a panic on the connection task.
fn col<'a, T: FromSql<'a>>(row: &'a Row, index: usize) -> Result<T> {
    row.try_get(index)
        .with_context(|| format!("reading column {index} of a catalog row"))
}

impl Db {
    /// Build the pool. Each new connection is scoped to the tenant (when one
    /// is configured) and given the statement timeout before it is handed out.
    pub(crate) fn connect(config: &DbConfig<'_>) -> Result<Self> {
        let pg = pgokf_pgconn::parse_config(config.database_url)?;
        let manager_config = ManagerConfig {
            recycling_method: RecyclingMethod::Fast,
        };
        let manager = if pgokf_pgconn::should_use_tls(pg.get_ssl_mode(), config.force_tls) {
            Manager::from_config(pg, pgokf_pgconn::rustls_connector()?, manager_config)
        } else {
            Manager::from_config(pg, NoTls, manager_config)
        };
        let tenant = config.tenant.map(str::to_owned);
        let timeout = config.statement_timeout_ms;
        let pool = Pool::builder(manager)
            .max_size(config.pool_size)
            .wait_timeout(Some(POOL_WAIT))
            .runtime(Runtime::Tokio1)
            .post_create(Hook::async_fn(move |client, _| {
                let tenant = tenant.clone();
                Box::pin(async move {
                    // statement_timeout takes no parameter, so the integer
                    // is formatted from a validated u64, never from input.
                    client
                        .batch_execute(&format!("SET statement_timeout = {timeout}"))
                        .await
                        .map_err(|error| HookError::message(error.to_string()))?;
                    if let Some(tenant) = tenant {
                        pgokf_pgconn::set_tenant(client, &tenant)
                            .await
                            .map_err(|error| HookError::message(error.to_string()))?;
                    }
                    Ok(())
                })
            }))
            .build()
            .context("building the PostgreSQL connection pool")?;
        Ok(Self { pool })
    }

    /// A pooled connection for the plugin builder, which runs its own
    /// statements through the reader role. The guard closes the connection
    /// instead of recycling it when the build is abandoned mid-flight
    /// (request timeout, client gone); call `finish` when it completed.
    pub(crate) async fn checkout(&self) -> Result<Borrowed> {
        self.client().await
    }

    async fn client(&self) -> Result<Borrowed> {
        let object = self
            .pool
            .get()
            .await
            .context("checking out a PostgreSQL connection")?;
        Ok(Borrowed {
            object: Some(object),
            done: false,
        })
    }

    async fn query(&self, sql: &str, params: &[&(dyn ToSql + Sync)]) -> Result<Vec<Row>> {
        let mut client = self.client().await?;
        let rows = client
            .get()
            .query(sql, params)
            .await
            .context("catalog query failed");
        client.finish();
        rows
    }

    async fn query_opt(&self, sql: &str, params: &[&(dyn ToSql + Sync)]) -> Result<Option<Row>> {
        let mut client = self.client().await?;
        let row = client
            .get()
            .query_opt(sql, params)
            .await
            .context("catalog query failed");
        client.finish();
        row
    }

    async fn query_map<T>(
        &self,
        sql: &str,
        params: &[&(dyn ToSql + Sync)],
        map: impl Fn(&Row) -> Result<T>,
    ) -> Result<Vec<T>> {
        self.query(sql, params).await?.iter().map(map).collect()
    }

    async fn json(&self, sql: &str) -> Result<Value> {
        let row = self
            .query_opt(sql, &[])
            .await?
            .ok_or_else(|| anyhow!("{sql} returned no row"))?;
        col(&row, 0)
    }

    /// `pgokf.health()`.
    pub(crate) async fn health(&self) -> Result<Value> {
        self.json("SELECT pgokf.health()").await
    }

    /// `pgokf.search_index_status()`.
    pub(crate) async fn index_status(&self) -> Result<Value> {
        self.json("SELECT pgokf.search_index_status()").await
    }

    /// `pgokf.get_config()`.
    pub(crate) async fn config(&self) -> Result<Value> {
        self.json("SELECT pgokf.get_config()").await
    }

    /// The loaded library version (`pgokf.version()`) and the installed SQL
    /// version, which must agree after an upgrade.
    pub(crate) async fn versions(&self) -> Result<(String, String)> {
        let row = self
            .query_opt(
                "SELECT pgokf.version(),
                        (SELECT extversion FROM pg_catalog.pg_extension WHERE extname = 'pgokf')",
                &[],
            )
            .await?
            .ok_or_else(|| anyhow!("version query returned no row"))?;
        Ok((
            col(&row, 0)?,
            col::<Option<String>>(&row, 1)?.unwrap_or_default(),
        ))
    }

    /// `pgokf.catalog_stats()`.
    pub(crate) async fn catalog_stats(&self) -> Result<Vec<BundleStat>> {
        let sql = format!(
            "SELECT s.bundle_id, {}, s.enabled, s.source_type, s.file_count, s.indexed_concepts,
                    s.link_count, s.resolved_link_count, {}, EXTRACT(EPOCH FROM s.sync_age)::bigint,
                    s.is_stale, {}
             FROM pgokf.catalog_stats() s JOIN pgokf.bundles b ON b.id = s.bundle_id
             ORDER BY 2, s.bundle_id",
            display_name("b"),
            iso("s.last_synced_at"),
            iso("s.retired_at")
        );
        self.query_map(&sql, &[], |r| {
            Ok(BundleStat {
                bundle_id: col(r, 0)?,
                name: col(r, 1)?,
                enabled: col(r, 2)?,
                source_type: col(r, 3)?,
                file_count: col(r, 4)?,
                indexed_concepts: col(r, 5)?,
                link_count: col(r, 6)?,
                resolved_link_count: col(r, 7)?,
                last_synced_at: col(r, 8)?,
                sync_age_seconds: col(r, 9)?,
                is_stale: col(r, 10)?,
                retired_at: col(r, 11)?,
            })
        })
        .await
    }

    fn bundles_sql(filter: &str) -> String {
        format!(
            "SELECT b.id, b.path, {}, b.okf_version, b.file_count, {}, b.enabled
             FROM pgokf.list_bundles() b {filter} ORDER BY 3, b.id",
            display_name("b"),
            iso("b.last_synced_at")
        )
    }

    /// `pgokf.list_bundles()`.
    pub(crate) async fn bundles(&self) -> Result<Vec<BundleInfo>> {
        self.query_map(&Self::bundles_sql(""), &[], bundle_info)
            .await
    }

    /// One bundle from `pgokf.list_bundles()`; `None` when it is not visible.
    pub(crate) async fn bundle(&self, id: i64) -> Result<Option<BundleInfo>> {
        self.query_opt(&Self::bundles_sql("WHERE b.id = $1"), &[&id])
            .await?
            .as_ref()
            .map(bundle_info)
            .transpose()
    }

    /// Every concept of one visible bundle (up to `cap`) for the builder's
    /// file picker: identity, path, type, title, and whether it is a skill
    /// package or a package member.
    pub(crate) async fn bundle_tree(&self, bundle_id: i64, cap: i64) -> Result<Vec<TreeEntry>> {
        self.query_map(
            "SELECT c.id, c.path, c.type, c.title,
                    (sk.concept_id IS NOT NULL),
                    coalesce(s.package_concept_id, d.package_concept_id),
                    coalesce(c.tags, '{}')
             FROM pgokf.concepts c
             JOIN pgokf.bundles b ON b.id = c.bundle_id AND b.enabled AND b.retired_at IS NULL
             LEFT JOIN pgokf.skills sk ON sk.bundle_id = c.bundle_id AND sk.concept_id = c.id
             LEFT JOIN pgokf.scripts s ON s.bundle_id = c.bundle_id AND s.concept_id = c.id
             LEFT JOIN pgokf.reference_documents d
                    ON d.bundle_id = c.bundle_id AND d.concept_id = c.id
             WHERE c.bundle_id = $1
             ORDER BY c.path
             LIMIT $2",
            &[&bundle_id, &cap],
            |r| {
                Ok(TreeEntry {
                    id: col(r, 0)?,
                    path: col(r, 1)?,
                    concept_type: col(r, 2)?,
                    title: col(r, 3)?,
                    package: col::<Option<bool>>(r, 4)?.unwrap_or(false),
                    package_of: col(r, 5)?,
                    tags: col(r, 6)?,
                })
            },
        )
        .await
    }

    /// Whether the catalog keeps document sources (`store_source`), which
    /// the human workflow needs to rebuild a content bundle.
    pub(crate) async fn keeps_sources(&self) -> Result<bool> {
        Ok(self.config().await?["store_source"]
            .as_bool()
            .unwrap_or(false))
    }

    /// The content bundles (registered from in-memory content, so the UI
    /// can resync them) that are enabled and not retired.
    pub(crate) async fn content_bundles(&self) -> Result<Vec<ContentBundle>> {
        self.query_map(
            "SELECT id, path, name, file_count
             FROM pgokf.bundles
             WHERE source_type = 'content' AND enabled AND retired_at IS NULL
             ORDER BY coalesce(name, path)",
            &[],
            |r| {
                let path: String = col(r, 1)?;
                Ok(ContentBundle {
                    id: col(r, 0)?,
                    name: content_bundle_name(&path, col::<Option<String>>(r, 2)?.as_deref()),
                    file_count: col(r, 3)?,
                })
            },
        )
        .await
    }

    /// One content bundle by id, or `None` when the bundle is not a content
    /// bundle (a filesystem or object-store bundle is edited at its source).
    pub(crate) async fn content_bundle(&self, id: i64) -> Result<Option<ContentBundle>> {
        let rows = self
            .query_map(
                "SELECT id, path, name, file_count
                 FROM pgokf.bundles
                 WHERE id = $1 AND source_type = 'content' AND enabled AND retired_at IS NULL",
                &[&id],
                |r| {
                    let path: String = col(r, 1)?;
                    Ok(ContentBundle {
                        id: col(r, 0)?,
                        name: content_bundle_name(&path, col::<Option<String>>(r, 2)?.as_deref()),
                        file_count: col(r, 3)?,
                    })
                },
            )
            .await?;
        Ok(rows.into_iter().next())
    }

    /// Every file of a bundle as the catalog stores it: document sources
    /// from `concept_source`, package files from the typed projections.
    ///
    /// # Errors
    ///
    /// A concept whose bytes are not stored (the catalog did not keep
    /// sources when it was ingested): the bundle cannot be rebuilt.
    pub(crate) async fn bundle_files(&self, bundle_id: i64) -> Result<Vec<BundleFile>> {
        let rows = self
            .query_map(
                "SELECT c.path, coalesce(sk.skill_md, sc.exact_bytes, rd.exact_bytes, s.raw_content)
                 FROM pgokf.concepts c
                 LEFT JOIN pgokf.concept_source s
                        ON s.bundle_id = c.bundle_id AND s.concept_id = c.id
                 LEFT JOIN pgokf.skills sk ON sk.bundle_id = c.bundle_id AND sk.concept_id = c.id
                 LEFT JOIN pgokf.scripts sc ON sc.bundle_id = c.bundle_id AND sc.concept_id = c.id
                 LEFT JOIN pgokf.reference_documents rd
                        ON rd.bundle_id = c.bundle_id AND rd.concept_id = c.id
                 WHERE c.bundle_id = $1
                 ORDER BY c.path",
                &[&bundle_id],
                |r| Ok((col::<String>(r, 0)?, col::<Option<Vec<u8>>>(r, 1)?)),
            )
            .await?;
        rows.into_iter()
            .map(|(path, bytes)| match bytes {
                Some(bytes) => Ok(BundleFile { path, bytes }),
                None => Err(anyhow!(
                    "the catalog holds no source for {path}; the bundle cannot be rebuilt \
                     (store_source was off when it was ingested)"
                )),
            })
            .collect()
    }

    /// Create or resync a content bundle from a full snapshot of its files
    /// (`pgokf.register_bundle_content`; a `pgokf_writer` connection).
    pub(crate) async fn register_content(
        &self,
        name: &str,
        files: &[BundleFile],
    ) -> Result<SyncOutcome> {
        let paths: Vec<&str> = files.iter().map(|f| f.path.as_str()).collect();
        let contents: Vec<&[u8]> = files.iter().map(|f| f.bytes.as_slice()).collect();
        let row = self
            .query_opt(
                "SELECT r.bundle_id, r.added, r.updated, r.removed
                 FROM pgokf.register_bundle_content($1, $2, $3, '{}'::jsonb) r",
                &[&name, &paths, &contents],
            )
            .await?
            .ok_or_else(|| anyhow!("register_bundle_content returned no row"))?;
        Ok(SyncOutcome {
            bundle_id: col(&row, 0)?,
            added: col::<Option<i32>>(&row, 1)?.unwrap_or_default(),
            updated: col::<Option<i32>>(&row, 2)?.unwrap_or_default(),
            removed: col::<Option<i32>>(&row, 3)?.unwrap_or_default(),
        })
    }

    /// Concepts of content bundles that no human has verified yet, oldest
    /// change first: the review queue.
    pub(crate) async fn review_queue(&self, limit: i64) -> Result<Vec<ReviewItem>> {
        let sql = format!(
            "SELECT c.bundle_id, {}, c.id, c.path, c.title, c.type, p.status,
                    coalesce(p.trust_tier, 'unverified'), p.generated_by, {}
             FROM pgokf.concepts c
             JOIN pgokf.bundles b ON b.id = c.bundle_id
                  AND b.source_type = 'content' AND b.enabled AND b.retired_at IS NULL
             LEFT JOIN pgokf.concept_provenance p
                    ON p.bundle_id = c.bundle_id AND p.concept_id = c.id
             WHERE coalesce(p.trust_tier, 'unverified') <> 'human-reviewed'
             ORDER BY coalesce(c.modified_at, c.indexed_at) NULLS FIRST, c.bundle_id, c.path
             LIMIT $1",
            display_name("b"),
            iso("coalesce(c.modified_at, c.indexed_at)")
        );
        self.query_map(&sql, &[&limit], |r| {
            Ok(ReviewItem {
                bundle_id: col(r, 0)?,
                bundle_name: col(r, 1)?,
                concept_id: col(r, 2)?,
                path: col(r, 3)?,
                title: col(r, 4)?,
                concept_type: col(r, 5)?,
                status: col(r, 6)?,
                trust_tier: col(r, 7)?,
                generated_by: col(r, 8)?,
                modified_at: col(r, 9)?,
            })
        })
        .await
    }

    /// Up to `limit` concepts of one bundle ordered by path, starting after
    /// `after_path` (keyset paging for large bundles).
    pub(crate) async fn bundle_concepts(
        &self,
        bundle_id: i64,
        after_path: Option<&str>,
        limit: i64,
    ) -> Result<Vec<ConceptSummary>> {
        let sql = format!(
            "SELECT bundle_id, id, path, type, title, description, coalesce(tags, '{{}}'), {}
             FROM pgokf.concepts
             WHERE bundle_id = $1 AND ($2::text IS NULL OR path > $2)
             ORDER BY path LIMIT $3",
            iso("modified_at")
        );
        self.query_map(&sql, &[&bundle_id, &after_path, &limit], concept_summary)
            .await
    }

    /// Concepts matching the filters alone (no query text): the browse mode
    /// behind a tag, type, or bundle link. Ordered by bundle then concept id
    /// (the cursor carries the id; path order differs from id order around
    /// `-` and `.`), and scoped like `concept_search`: enabled, unretired
    /// bundles and the provenance row's own status and trust tier.
    pub(crate) async fn browse(
        &self,
        q: &SearchQuery,
        after: Option<(i64, &str)>,
        limit: i64,
    ) -> Result<Vec<ConceptSummary>> {
        let sql = format!(
            "SELECT c.bundle_id, c.id, c.path, c.type, c.title, c.description,
                    coalesce(c.tags, '{{}}'), {}
             FROM pgokf.concepts c
             JOIN pgokf.bundles b
                    ON b.id = c.bundle_id AND b.enabled AND b.retired_at IS NULL
             LEFT JOIN pgokf.concept_provenance p
                    ON p.bundle_id = c.bundle_id AND p.concept_id = c.id
             WHERE ($1::bigint IS NULL OR c.bundle_id = $1)
               AND ($2::text IS NULL OR c.type = $2)
               AND ($3::text[] IS NULL OR c.tags @> $3)
               AND ($4::text IS NULL OR p.status = $4)
               AND ($5::text IS NULL OR p.trust_tier = $5)
               AND ($6::bigint IS NULL OR (c.bundle_id, c.id) > ($6, $7))
             ORDER BY c.bundle_id, c.id LIMIT $8",
            iso("c.modified_at")
        );
        let (after_bundle, after_id) = match after {
            Some((bundle, id)) => (Some(bundle), Some(id)),
            None => (None, None),
        };
        self.query_map(
            &sql,
            &[
                &q.bundle_id,
                &q.concept_type,
                &q.tags_param(),
                &q.status,
                &q.trust_tier,
                &after_bundle,
                &after_id,
                &limit,
            ],
            concept_summary,
        )
        .await
    }

    /// `pgokf.list_sync_log(bundle_id, max_rows)`.
    pub(crate) async fn sync_log(
        &self,
        bundle_id: Option<i64>,
        max_rows: i32,
    ) -> Result<Vec<SyncLogEntry>> {
        let sql = format!(
            "SELECT id, bundle_id, bundle_path, op, actor, {}, added, updated, removed, unchanged, total
             FROM pgokf.list_sync_log($1, $2)",
            iso("synced_at")
        );
        self.query_map(&sql, &[&bundle_id, &max_rows], |r| {
            Ok(SyncLogEntry {
                id: col(r, 0)?,
                bundle_id: col(r, 1)?,
                bundle_path: col(r, 2)?,
                op: col(r, 3)?,
                actor: col(r, 4)?,
                synced_at: col(r, 5)?,
                added: col(r, 6)?,
                updated: col(r, 7)?,
                removed: col(r, 8)?,
                unchanged: col(r, 9)?,
                total: col(r, 10)?,
            })
        })
        .await
    }

    /// `pgokf.list_bundle_log(bundle_id, NULL, max_rows)`.
    pub(crate) async fn bundle_log(
        &self,
        bundle_id: i64,
        max_rows: i32,
    ) -> Result<Vec<BundleLogEntry>> {
        let sql = format!(
            "SELECT directory, ordinal, {}, entry FROM pgokf.list_bundle_log($1, NULL, $2)",
            iso("logged_at")
        );
        self.query_map(&sql, &[&bundle_id, &max_rows], |r| {
            Ok(BundleLogEntry {
                directory: col(r, 0)?,
                ordinal: col(r, 1)?,
                logged_at: col(r, 2)?,
                entry: col(r, 3)?,
            })
        })
        .await
    }

    /// Lexical search through `pgokf.concept_search`, asking for `limit`
    /// rows (the caller passes one more than the page to detect a next page).
    pub(crate) async fn search(&self, q: &SearchQuery, limit: i32) -> Result<Vec<Hit>> {
        let cursor = q.after.as_ref().map(|c| {
            serde_json::json!({"rank": c.rank, "bundle_id": c.bundle_id, "concept_id": c.concept_id})
        });
        self.query_map(
            "SELECT bundle_id, concept_id, path, title, type, rank, headline
             FROM pgokf.concept_search($1, $2, $3, $4, $5, $6, $7, $8)",
            &[
                &q.query,
                &q.bundle_id,
                &limit,
                &q.concept_type,
                &q.tags_param(),
                &q.status,
                &q.trust_tier,
                &cursor,
            ],
            hit,
        )
        .await
    }

    /// Semantic (`concept_search_semantic`) or hybrid (`concept_search_hybrid`)
    /// search with a caller-supplied query embedding.
    pub(crate) async fn search_with_embedding(
        &self,
        q: &SearchQuery,
        embedding: &[f32],
        hybrid: bool,
    ) -> Result<Vec<Hit>> {
        let embedding: Vec<f32> = embedding.to_vec();
        if hybrid {
            self.query_map(
                "SELECT bundle_id, concept_id, path, title, type, rank, headline
                 FROM pgokf.concept_search_hybrid($1, $2, $3, $4)",
                &[&q.query, &embedding, &q.bundle_id, &q.limit],
                hit,
            )
            .await
        } else {
            self.query_map(
                "SELECT bundle_id, concept_id, path, title, type, rank, headline
                 FROM pgokf.concept_search_semantic($1, $2, $3)",
                &[&embedding, &q.bundle_id, &q.limit],
                hit,
            )
            .await
        }
    }

    /// `pgokf.search_facets` for one facet name (`type`, `bundle`, `tag`,
    /// `status`, `trust_tier`) over the query's matches.
    pub(crate) async fn facets(&self, q: &SearchQuery, facet: &str) -> Result<Vec<Facet>> {
        self.query_map(
            "SELECT facet_value, count
             FROM pgokf.search_facets($1, $2, $3, $4, $5, $6, $7)
             ORDER BY count DESC, facet_value",
            &[
                &q.query,
                &q.bundle_id,
                &facet,
                &q.concept_type,
                &q.tags_param(),
                &q.status,
                &q.trust_tier,
            ],
            facet_row,
        )
        .await
    }

    /// Facets over the whole visible catalog (optionally one bundle), for
    /// the search page before a query is typed; `search_facets` needs query
    /// text, so these read the projection tables directly.
    pub(crate) async fn catalog_facets(
        &self,
        bundle_id: Option<i64>,
        facet: &str,
    ) -> Result<Vec<Facet>> {
        let (expr, from) = match facet {
            "type" => ("c.type", ""),
            "tag" => ("t.tag", ", LATERAL unnest(c.tags) AS t(tag)"),
            "status" => (
                "p.status",
                " LEFT JOIN pgokf.concept_provenance p ON p.bundle_id = c.bundle_id AND p.concept_id = c.id",
            ),
            "trust_tier" => (
                "p.trust_tier",
                " LEFT JOIN pgokf.concept_provenance p ON p.bundle_id = c.bundle_id AND p.concept_id = c.id",
            ),
            _ => return Err(anyhow!("unknown catalog facet {facet}")),
        };
        let sql = format!(
            "SELECT {expr}, count(*)::bigint
             FROM pgokf.concepts c
             JOIN pgokf.bundles b ON b.id = c.bundle_id AND b.enabled AND b.retired_at IS NULL{from}
             WHERE ($1::bigint IS NULL OR c.bundle_id = $1) AND {expr} IS NOT NULL
             GROUP BY 1 ORDER BY 2 DESC, 1 LIMIT 100"
        );
        self.query_map(&sql, &[&bundle_id], facet_row).await
    }

    /// One concept with its metadata and provenance; `None` when it is not
    /// visible to this session (missing, other tenant, disabled or retired
    /// bundle are indistinguishable by design, as in `concept_search`).
    pub(crate) async fn concept(
        &self,
        bundle_id: i64,
        concept_id: &str,
    ) -> Result<Option<ConceptDetail>> {
        let sql = format!(
            "SELECT c.bundle_id, {}, c.id, c.path, c.type, c.title, c.description,
                    coalesce(c.tags, '{{}}'), c.resource, c.body_text, c.file_hash,
                    {}, {}, c.tenant_id, s.raw_content
             FROM pgokf.concepts c
             JOIN pgokf.bundles b ON b.id = c.bundle_id AND b.retired_at IS NULL
             LEFT JOIN pgokf.concept_source s
                    ON s.bundle_id = c.bundle_id AND s.concept_id = c.id
             WHERE c.bundle_id = $1 AND c.id = $2",
            display_name("b"),
            iso("c.modified_at"),
            iso("c.indexed_at")
        );
        let Some(row) = self.query_opt(&sql, &[&bundle_id, &concept_id]).await? else {
            return Ok(None);
        };
        let metadata = self
            .query_map(
                "SELECT key, value FROM pgokf.concept_metadata
                 WHERE bundle_id = $1 AND concept_id = $2 ORDER BY key",
                &[&bundle_id, &concept_id],
                |r| Ok((col::<String>(r, 0)?, col::<Value>(r, 1)?)),
            )
            .await?
            .into_iter()
            .collect();
        let provenance = self.provenance(bundle_id, concept_id).await?;
        let verifications = self.verifications(bundle_id, concept_id).await?;
        let sources = self.provenance_sources(bundle_id, concept_id).await?;
        let source: Option<Vec<u8>> = col(&row, 14)?;
        Ok(Some(ConceptDetail {
            bundle_id: col(&row, 0)?,
            bundle_name: col(&row, 1)?,
            concept_id: col(&row, 2)?,
            path: col(&row, 3)?,
            concept_type: col(&row, 4)?,
            title: col(&row, 5)?,
            description: col(&row, 6)?,
            tags: col(&row, 7)?,
            resource: col(&row, 8)?,
            body_text: col(&row, 9)?,
            file_hash: col(&row, 10)?,
            modified_at: col(&row, 11)?,
            indexed_at: col(&row, 12)?,
            tenant_id: col(&row, 13)?,
            metadata,
            provenance,
            verifications,
            sources,
            has_source: source.is_some(),
            source,
        }))
    }

    async fn provenance(&self, bundle_id: i64, concept_id: &str) -> Result<Option<Provenance>> {
        let sql = format!(
            "SELECT generated_by, {}, status, {}, {}, {}, trust_tier, details
             FROM pgokf.concept_provenance WHERE bundle_id = $1 AND concept_id = $2",
            iso("generated_at"),
            iso("stale_after"),
            iso("usage_window_from"),
            iso("usage_window_to")
        );
        self.query_opt(&sql, &[&bundle_id, &concept_id])
            .await?
            .map(|r| {
                Ok(Provenance {
                    generated_by: col(&r, 0)?,
                    generated_at: col(&r, 1)?,
                    status: col(&r, 2)?,
                    stale_after: col(&r, 3)?,
                    usage_window_from: col(&r, 4)?,
                    usage_window_to: col(&r, 5)?,
                    trust_tier: col(&r, 6)?,
                    details: col::<Option<Value>>(&r, 7)?.unwrap_or(Value::Null),
                })
            })
            .transpose()
    }

    async fn verifications(&self, bundle_id: i64, concept_id: &str) -> Result<Vec<Verification>> {
        let sql = format!(
            "SELECT ordinal, verified_by, {} FROM pgokf.concept_verification
             WHERE bundle_id = $1 AND concept_id = $2 ORDER BY ordinal",
            iso("verified_at")
        );
        self.query_map(&sql, &[&bundle_id, &concept_id], |r| {
            Ok(Verification {
                ordinal: col(r, 0)?,
                verified_by: col(r, 1)?,
                verified_at: col(r, 2)?,
            })
        })
        .await
    }

    async fn provenance_sources(
        &self,
        bundle_id: i64,
        concept_id: &str,
    ) -> Result<Vec<ProvenanceSource>> {
        let sql = format!(
            "SELECT ordinal, source_id, resource, title, author, usage_count, {}
             FROM pgokf.concept_provenance_source
             WHERE bundle_id = $1 AND concept_id = $2 ORDER BY ordinal",
            iso("last_modified")
        );
        self.query_map(&sql, &[&bundle_id, &concept_id], |r| {
            Ok(ProvenanceSource {
                ordinal: col(r, 0)?,
                source_id: col(r, 1)?,
                resource: col(r, 2)?,
                title: col(r, 3)?,
                author: col(r, 4)?,
                usage_count: col(r, 5)?,
                last_modified: col(r, 6)?,
            })
        })
        .await
    }

    /// Outgoing and incoming links of one concept.
    pub(crate) async fn links(
        &self,
        bundle_id: i64,
        concept_id: &str,
    ) -> Result<(Vec<Link>, Vec<Link>)> {
        let outgoing = self
            .query_map(
                "SELECT l.bundle_id, l.source_id, l.target_id, l.link_text, l.target_path,
                        l.link_kind, l.link_relation, l.resolved, l.is_external, t.title
                 FROM pgokf.links l
                 LEFT JOIN pgokf.concepts t
                        ON t.bundle_id = l.bundle_id AND t.id = l.target_id
                 WHERE l.bundle_id = $1 AND l.source_id = $2
                 ORDER BY l.ordinal",
                &[&bundle_id, &concept_id],
                link,
            )
            .await?;
        let incoming = self
            .query_map(
                "SELECT l.bundle_id, l.source_id, l.target_id, l.link_text, l.target_path,
                        l.link_kind, l.link_relation, l.resolved, l.is_external, s.title
                 FROM pgokf.links l
                 LEFT JOIN pgokf.concepts s
                        ON s.bundle_id = l.bundle_id AND s.id = l.source_id
                 WHERE l.bundle_id = $1 AND l.target_id = $2
                 ORDER BY l.source_id, l.ordinal",
                &[&bundle_id, &concept_id],
                link,
            )
            .await?;
        Ok((outgoing, incoming))
    }

    /// `pgokf.concept_neighbors(concept_id, max_hops, bundle_id)`.
    pub(crate) async fn neighbors(
        &self,
        bundle_id: i64,
        concept_id: &str,
        max_hops: i32,
    ) -> Result<Vec<Neighbor>> {
        self.query_map(
            "SELECT neighbor_id, title, hops, path
             FROM pgokf.concept_neighbors($1, $2, $3) ORDER BY hops, neighbor_id",
            &[&concept_id, &max_hops, &bundle_id],
            |r| {
                Ok(Neighbor {
                    bundle_id,
                    id: col(r, 0)?,
                    title: col(r, 1)?,
                    hops: col(r, 2)?,
                    path: col::<Option<Vec<String>>>(r, 3)?.unwrap_or_default(),
                })
            },
        )
        .await
    }

    /// The neighborhood graph of one concept: the seed plus every concept
    /// `pgokf.concept_neighbors()` reaches within `max_hops`, and the
    /// resolved links among that set. Empty when the seed is not visible.
    pub(crate) async fn graph(
        &self,
        bundle_id: i64,
        concept_id: &str,
        max_hops: i32,
    ) -> Result<Graph> {
        let sql = format!(
            "WITH n AS (
                 SELECT $1::text AS id, 0 AS hops
                 UNION ALL
                 SELECT neighbor_id, hops FROM pgokf.concept_neighbors($1, $2, $3)
             )
             SELECT c.bundle_id, {}, c.id, c.title, c.type, c.path, min(n.hops)::int,
                    (SELECT count(*) FROM pgokf.links l
                      WHERE l.bundle_id = c.bundle_id AND l.resolved AND l.source_id <> l.target_id
                        AND (l.source_id = c.id OR l.target_id = c.id))
             FROM n
             JOIN pgokf.concepts c ON c.bundle_id = $3 AND c.id = n.id
             JOIN pgokf.bundles b ON b.id = c.bundle_id
             GROUP BY c.bundle_id, b.name, b.path, c.id, c.title, c.type, c.path
             ORDER BY 7, c.id",
            display_name("b")
        );
        let nodes = self
            .query_map(&sql, &[&concept_id, &max_hops, &bundle_id], graph_node)
            .await?;
        let total = i64::try_from(nodes.len()).unwrap_or(i64::MAX);
        let links = self.links_among(&nodes).await?;
        Ok(Graph {
            nodes,
            links,
            total,
        })
    }

    /// The catalog-wide graph: the `limit` best-connected visible concepts
    /// (of one bundle, or all) and the resolved links among them. Degrees
    /// come from one aggregate over the links, not a probe per concept.
    pub(crate) async fn catalog_graph(&self, bundle_id: Option<i64>, limit: i64) -> Result<Graph> {
        let sql = format!(
            "WITH ends AS (
                 SELECT l.bundle_id, l.source_id AS id FROM pgokf.links l
                  WHERE l.resolved AND l.source_id <> l.target_id
                    AND ($1::bigint IS NULL OR l.bundle_id = $1)
                 UNION ALL
                 SELECT l.bundle_id, l.target_id FROM pgokf.links l
                  WHERE l.resolved AND l.source_id <> l.target_id
                    AND ($1::bigint IS NULL OR l.bundle_id = $1)
             ), deg AS (
                 SELECT bundle_id, id, count(*)::bigint AS degree FROM ends GROUP BY 1, 2
             ), visible AS (
                 SELECT c.bundle_id, c.id, c.title, c.type, c.path, {} AS bundle_name,
                        coalesce(deg.degree, 0) AS degree
                 FROM pgokf.concepts c
                 JOIN pgokf.bundles b ON b.id = c.bundle_id AND b.enabled AND b.retired_at IS NULL
                 LEFT JOIN deg ON deg.bundle_id = c.bundle_id AND deg.id = c.id
                 WHERE ($1::bigint IS NULL OR c.bundle_id = $1)
             )
             SELECT bundle_id, bundle_name, id, title, type, path, 0::int, degree,
                    count(*) OVER ()
             FROM visible
             ORDER BY degree DESC, bundle_id, id
             LIMIT $2",
            display_name("b")
        );
        let rows = self.query(&sql, &[&bundle_id, &limit]).await?;
        let total = rows
            .first()
            .map(|r| col::<i64>(r, 8))
            .transpose()?
            .unwrap_or(0);
        let nodes = rows.iter().map(graph_node).collect::<Result<Vec<_>>>()?;
        let links = self.links_among(&nodes).await?;
        Ok(Graph {
            nodes,
            links,
            total,
        })
    }

    /// The resolved links whose both ends are in `nodes` (self-links, which
    /// are in-page anchors, excluded), folded per pair.
    async fn links_among(&self, nodes: &[GraphNode]) -> Result<Vec<GraphLink>> {
        if nodes.is_empty() {
            return Ok(Vec::new());
        }
        let bundles: Vec<i64> = nodes.iter().map(|n| n.bundle_id).collect();
        let ids: Vec<&str> = nodes.iter().map(|n| n.id.as_str()).collect();
        self.query_map(
            "WITH n AS (SELECT * FROM ROWS FROM (unnest($1::bigint[]), unnest($2::text[])) AS t(bundle_id, id))
             SELECT l.bundle_id, l.source_id, l.target_id, count(*)::bigint,
                    array_agg(DISTINCT l.link_relation ORDER BY l.link_relation),
                    array_agg(DISTINCT l.link_text ORDER BY l.link_text)
             FROM pgokf.links l
             JOIN n s ON s.bundle_id = l.bundle_id AND s.id = l.source_id
             JOIN n t ON t.bundle_id = l.bundle_id AND t.id = l.target_id
             WHERE l.resolved AND l.source_id <> l.target_id
             GROUP BY 1, 2, 3 ORDER BY 1, 2, 3",
            &[&bundles, &ids],
            |r| {
                Ok(GraphLink {
                    bundle_id: col(r, 0)?,
                    source: col(r, 1)?,
                    target: col(r, 2)?,
                    count: col(r, 3)?,
                    relations: col::<Option<Vec<Option<String>>>>(r, 4)?
                        .unwrap_or_default()
                        .into_iter()
                        .flatten()
                        .collect(),
                    texts: col::<Option<Vec<Option<String>>>>(r, 5)?
                        .unwrap_or_default()
                        .into_iter()
                        .flatten()
                        .filter(|t| !t.trim().is_empty())
                        .take(EDGE_TEXTS)
                        .collect(),
                })
            },
        )
        .await
    }

    /// `pgokf.find_similar(concept_id, bundle_id, limit)`.
    pub(crate) async fn similar(
        &self,
        bundle_id: i64,
        concept_id: &str,
        limit: i32,
    ) -> Result<Vec<Hit>> {
        self.query_map(
            "SELECT bundle_id, concept_id, path, title, type, rank, headline
             FROM pgokf.find_similar($1, $2, $3)",
            &[&concept_id, &bundle_id, &limit],
            hit,
        )
        .await
    }

    /// `pgokf.concept_history(bundle_id, concept_id, max_rows)`.
    pub(crate) async fn history(
        &self,
        bundle_id: i64,
        concept_id: &str,
        max_rows: i32,
    ) -> Result<Vec<Version>> {
        let sql = format!(
            "SELECT version, {}, {}, change_kind, type, title, file_hash
             FROM pgokf.concept_history($1, $2, $3)",
            iso("valid_from"),
            iso("valid_to")
        );
        self.query_map(&sql, &[&bundle_id, &concept_id, &max_rows], |r| {
            Ok(Version {
                number: col(r, 0)?,
                valid_from: col(r, 1)?,
                valid_to: col(r, 2)?,
                change_kind: col(r, 3)?,
                concept_type: col(r, 4)?,
                title: col(r, 5)?,
                file_hash: col(r, 6)?,
            })
        })
        .await
    }

    /// The skill package a concept is the manifest of, with its resources,
    /// from the projection tables (an unaudited read: no bytes).
    pub(crate) async fn package(
        &self,
        bundle_id: i64,
        concept_id: &str,
    ) -> Result<Option<PackageInfo>> {
        let Some(row) = self
            .query_opt(
                "SELECT coalesce(agent_skill->>'name', ''), package_root, package_hash, visibility
                 FROM pgokf.skills WHERE bundle_id = $1 AND concept_id = $2",
                &[&bundle_id, &concept_id],
            )
            .await?
        else {
            return Ok(None);
        };
        let resources = self
            .query_map(
                &format!(
                    "SELECT x.concept_id, x.class, x.path, x.byte_size, x.sha256, x.detail,
                            x.package_concept_id, x.textual, NULL::text
                     FROM ({RESOURCE_ROWS} WHERE r.bundle_id = $1 AND r.package_concept_id = $2) x
                     ORDER BY x.path"
                ),
                &[&bundle_id, &concept_id],
                resource,
            )
            .await?;
        Ok(Some(PackageInfo {
            name: col(&row, 0)?,
            root: col(&row, 1)?,
            hash: col(&row, 2)?,
            visibility: col(&row, 3)?,
            resources,
        }))
    }

    /// The resources of several packages at once (one query), keyed by
    /// `(bundle_id, skill concept id)`, without the text: what a preview
    /// needs to list every file of every selected package.
    pub(crate) async fn package_resources(
        &self,
        packages: &[(i64, String)],
    ) -> Result<BTreeMap<(i64, String), Vec<ResourceInfo>>> {
        let bundle_ids: Vec<i64> = packages.iter().map(|(b, _)| *b).collect();
        let skill_ids: Vec<String> = packages.iter().map(|(_, id)| id.clone()).collect();
        let rows = self
            .query_map(
                &format!(
                    "SELECT x.concept_id, x.class, x.path, x.byte_size, x.sha256, x.detail,
                            x.package_concept_id, x.textual, NULL::text, x.bundle_id
                     FROM ({RESOURCE_ROWS}
                           JOIN ROWS FROM (unnest($1::bigint[]), unnest($2::text[])) AS k(b, id)
                             ON k.b = r.bundle_id AND k.id = r.package_concept_id) x
                     ORDER BY x.bundle_id, x.package_concept_id, x.path"
                ),
                &[&bundle_ids, &skill_ids],
                |row| Ok((col::<i64>(row, 9)?, resource(row)?)),
            )
            .await?;
        let mut grouped: BTreeMap<(i64, String), Vec<ResourceInfo>> = BTreeMap::new();
        for (bundle_id, info) in rows {
            grouped
                .entry((bundle_id, info.package_concept_id.clone()))
                .or_default()
                .push(info);
        }
        Ok(grouped)
    }

    /// The package resource a concept is, when it is one.
    pub(crate) async fn resource(
        &self,
        bundle_id: i64,
        concept_id: &str,
    ) -> Result<Option<ResourceInfo>> {
        self.query_opt(
            &format!("{RESOURCE_ROWS} WHERE r.bundle_id = $1 AND r.concept_id = $2"),
            &[&bundle_id, &concept_id],
        )
        .await?
        .as_ref()
        .map(resource)
        .transpose()
    }

    /// The exact stored bytes of a package manifest, script, reference, or
    /// asset with its media type, through the audited readers (`get_skill`,
    /// `get_script`, `get_reference`); `None` when the concept is none of
    /// those, or its bundle is retired (the page rule). This is the download
    /// path, not the page path.
    pub(crate) async fn exact_bytes(
        &self,
        bundle_id: i64,
        concept_id: &str,
    ) -> Result<Option<ExactBytes>> {
        let Some(row) = self
            .query_opt(
                "SELECT CASE WHEN sk.concept_id IS NOT NULL THEN 'skill'
                             WHEN s.concept_id IS NOT NULL THEN 'script'
                             WHEN d.concept_id IS NOT NULL THEN 'reference' END,
                        coalesce(d.media_type, 'text/plain')
                 FROM pgokf.concepts c
                 JOIN pgokf.bundles b ON b.id = c.bundle_id AND b.retired_at IS NULL
                 LEFT JOIN pgokf.skills sk ON sk.bundle_id = c.bundle_id AND sk.concept_id = c.id
                 LEFT JOIN pgokf.scripts s ON s.bundle_id = c.bundle_id AND s.concept_id = c.id
                 LEFT JOIN pgokf.reference_documents d
                        ON d.bundle_id = c.bundle_id AND d.concept_id = c.id
                 WHERE c.bundle_id = $1 AND c.id = $2",
                &[&bundle_id, &concept_id],
            )
            .await?
        else {
            return Ok(None);
        };
        let (sql, media_type, is_manifest) = match col::<Option<String>>(&row, 0)?.as_deref() {
            Some("skill") => (
                "SELECT (pgokf.get_skill($1, $2)).skill_md",
                "text/markdown".to_owned(),
                true,
            ),
            Some("script") => (
                "SELECT (pgokf.get_script($1, $2)).exact_bytes",
                "text/plain".to_owned(),
                false,
            ),
            Some("reference") => (
                "SELECT (pgokf.get_reference($1, $2)).exact_bytes",
                col::<String>(&row, 1)?,
                false,
            ),
            _ => return Ok(None),
        };
        let row = self
            .query_opt(sql, &[&bundle_id, &concept_id])
            .await?
            .ok_or_else(|| anyhow!("the package reader returned no row"))?;
        Ok(Some(ExactBytes {
            bytes: col(&row, 0)?,
            media_type,
            is_manifest,
        }))
    }

    /// `pgokf.get_concept_source(bundle_id, concept_id)`: the exact stored
    /// bytes for a download. This is the audited read (the extension logs
    /// it), which is why page rendering does not use it.
    pub(crate) async fn source_bytes(&self, bundle_id: i64, concept_id: &str) -> Result<Vec<u8>> {
        let row = self
            .query_opt(
                "SELECT pgokf.get_concept_source($1, $2)",
                &[&bundle_id, &concept_id],
            )
            .await?
            .ok_or_else(|| anyhow!("get_concept_source returned no row"))?;
        col(&row, 0)
    }

    /// `pgokf.stale_concepts(NULL, NULL)` with each concept's title.
    pub(crate) async fn stale(&self) -> Result<Vec<StaleConcept>> {
        let sql = format!(
            "SELECT s.bundle_id, s.concept_id, s.path, c.title, {}
             FROM pgokf.stale_concepts(NULL, NULL) s
             LEFT JOIN pgokf.concepts c ON c.bundle_id = s.bundle_id AND c.id = s.concept_id
             ORDER BY s.stale_after LIMIT 200",
            iso("s.stale_after")
        );
        self.query_map(&sql, &[], |r| {
            Ok(StaleConcept {
                bundle_id: col(r, 0)?,
                concept_id: col(r, 1)?,
                path: col(r, 2)?,
                title: col(r, 3)?,
                stale_after: col(r, 4)?,
            })
        })
        .await
    }

    /// `pgokf.duplicate_concepts(NULL, 2)`.
    pub(crate) async fn duplicates(&self) -> Result<Vec<DuplicateGroup>> {
        self.query_map(
            "SELECT file_hash, occurrences, bundle_ids, concept_ids
             FROM pgokf.duplicate_concepts(NULL, 2) ORDER BY occurrences DESC LIMIT 200",
            &[],
            |r| {
                Ok(DuplicateGroup {
                    file_hash: col(r, 0)?,
                    occurrences: col(r, 1)?,
                    bundle_ids: col::<Option<Vec<i64>>>(r, 2)?.unwrap_or_default(),
                    concept_ids: col::<Option<Vec<String>>>(r, 3)?.unwrap_or_default(),
                })
            },
        )
        .await
    }
}

/// A pooled connection for one statement. A statement that completes (or
/// fails) hands the connection back to the pool; one that is abandoned
/// mid-flight (the request timed out or the client went away) takes the
/// connection out of the pool and closes it instead, so the server aborts
/// the statement and no later request queues behind it.
pub(crate) struct Borrowed {
    object: Option<Object>,
    done: bool,
}

impl Borrowed {
    fn get(&self) -> &Object {
        self.object
            .as_ref()
            .expect("a borrowed connection is present until finish or drop")
    }

    /// The underlying client, for callers that run their own statements.
    pub(crate) fn client(&self) -> &tokio_postgres::Client {
        self.get()
    }

    /// Mark the work complete so the connection returns to the pool.
    pub(crate) fn finish(&mut self) {
        self.done = true;
    }
}

impl Drop for Borrowed {
    fn drop(&mut self) {
        if !self.done
            && let Some(object) = self.object.take()
        {
            // Detach from the pool and drop the client: closing the socket
            // is what makes the server cancel the running statement.
            drop(Object::take(object));
        }
    }
}

fn graph_node(r: &Row) -> Result<GraphNode> {
    Ok(GraphNode {
        bundle_id: col(r, 0)?,
        bundle_name: col(r, 1)?,
        id: col(r, 2)?,
        title: col(r, 3)?,
        concept_type: col(r, 4)?,
        path: col(r, 5)?,
        hops: col(r, 6)?,
        degree: col(r, 7)?,
    })
}

fn bundle_info(r: &Row) -> Result<BundleInfo> {
    Ok(BundleInfo {
        id: col(r, 0)?,
        path: col(r, 1)?,
        name: col(r, 2)?,
        okf_version: col(r, 3)?,
        file_count: col(r, 4)?,
        last_synced_at: col(r, 5)?,
        enabled: col(r, 6)?,
    })
}

fn concept_summary(r: &Row) -> Result<ConceptSummary> {
    Ok(ConceptSummary {
        bundle_id: col(r, 0)?,
        concept_id: col(r, 1)?,
        path: col(r, 2)?,
        concept_type: col(r, 3)?,
        title: col(r, 4)?,
        description: col(r, 5)?,
        tags: col(r, 6)?,
        modified_at: col(r, 7)?,
    })
}

fn hit(r: &Row) -> Result<Hit> {
    Ok(Hit {
        bundle_id: col(r, 0)?,
        concept_id: col(r, 1)?,
        path: col(r, 2)?,
        title: col(r, 3)?,
        concept_type: col(r, 4)?,
        rank: col::<Option<f32>>(r, 5)?.unwrap_or(0.0),
        headline: col(r, 6)?,
    })
}

fn facet_row(r: &Row) -> Result<Facet> {
    Ok(Facet {
        value: col(r, 0)?,
        count: col(r, 1)?,
    })
}

/// The union of both resource tables in one column shape (the nine columns
/// [`resource`] reads, then `bundle_id` for grouping).
const RESOURCE_ROWS: &str = "SELECT r.concept_id, r.class, r.path, r.byte_size, r.sha256, r.detail,
                                    r.package_concept_id, r.textual, r.text, r.bundle_id
                             FROM (
                                 SELECT bundle_id, concept_id, 'script' AS class, source_path AS path,
                                        byte_size, executable_sha256 AS sha256, language AS detail,
                                        package_concept_id, true AS textual, NULL::text AS text
                                 FROM pgokf.scripts
                                 UNION ALL
                                 SELECT bundle_id, concept_id,
                                        CASE WHEN source_path LIKE 'assets/%' THEN 'asset' ELSE 'reference' END,
                                        source_path, byte_size, content_sha256, media_type,
                                        package_concept_id, text_body IS NOT NULL, text_body
                                 FROM pgokf.reference_documents
                             ) AS r";

fn resource(r: &Row) -> Result<ResourceInfo> {
    Ok(ResourceInfo {
        concept_id: col(r, 0)?,
        class: col(r, 1)?,
        path: col(r, 2)?,
        byte_size: col::<Option<i64>>(r, 3)?.unwrap_or(0),
        sha256: col(r, 4)?,
        detail: col::<Option<String>>(r, 5)?.unwrap_or_default(),
        package_concept_id: col::<Option<String>>(r, 6)?.unwrap_or_default(),
        textual: col::<Option<bool>>(r, 7)?.unwrap_or(false),
        text: col(r, 8)?,
    })
}

fn link(r: &Row) -> Result<Link> {
    Ok(Link {
        bundle_id: col(r, 0)?,
        source_id: col(r, 1)?,
        target_id: col(r, 2)?,
        text: col(r, 3)?,
        target_path: col(r, 4)?,
        kind: col::<Option<String>>(r, 5)?.unwrap_or_default(),
        relation: col::<Option<String>>(r, 6)?.unwrap_or_default(),
        resolved: col::<Option<bool>>(r, 7)?.unwrap_or(false),
        is_external: col::<Option<bool>>(r, 8)?.unwrap_or(false),
        counterpart_title: col(r, 9)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn iso_substitutes_the_column_into_the_utc_rendering() {
        // Arrange & Act
        let expression = iso("c.modified_at");

        // Assert
        assert!(expression.starts_with("to_char(c.modified_at AT TIME ZONE 'UTC'"));
        assert!(expression.ends_with("\"Z\"')"));
    }

    #[test]
    fn display_name_falls_back_to_the_last_path_segment() {
        // Arrange & Act
        let expression = display_name("b");

        // Assert
        assert_eq!(
            expression,
            "coalesce(b.name, regexp_replace(b.path, '^.*/', ''))"
        );
    }

    #[test]
    fn classify_recognises_pool_timeouts_and_plain_errors() {
        // Arrange
        let busy = anyhow::Error::new(PoolError::Timeout(deadpool_postgres::TimeoutType::Wait))
            .context("checking out a PostgreSQL connection");
        let other = anyhow!("template rendering failed");

        // Act & Assert
        assert_eq!(classify(&busy), Failure::Busy);
        assert_eq!(classify(&other), Failure::Other);
        assert_eq!(db_message(&other), None);
    }

    #[test]
    fn has_filters_ignores_query_text_and_paging() {
        // Arrange
        let bare = SearchQuery {
            query: "x".to_owned(),
            limit: 20,
            ..SearchQuery::default()
        };
        let tagged = SearchQuery {
            tags: vec!["a".to_owned()],
            ..bare.clone()
        };

        // Act & Assert
        assert!(!bare.has_filters());
        assert!(tagged.has_filters());
    }
}
