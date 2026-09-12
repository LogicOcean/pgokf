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
/// How long a content change waits for another writer of the same bundle
/// before giving up, in milliseconds as `lock_timeout` takes it. A writer
/// that has wedged holding the lock must not stop every other one for ever.
const LOCK_WAIT: &str = "15000";

/// A pooled reader connection to one catalog.
#[derive(Clone)]
pub(crate) struct Db {
    pool: Pool,
}

fn personal_item(r: &Row) -> Result<PersonalItem> {
    Ok(PersonalItem {
        bundle_id: col(r, 0)?,
        bundle_name: col(r, 1)?,
        concept_id: col(r, 2)?,
        path: col(r, 3)?,
        title: col(r, 4)?,
        concept_type: col(r, 5)?,
        trust_tier: col(r, 6)?,
        when: col(r, 7)?,
    })
}

/// The name a content bundle is keyed on: its registered name, or the
/// synthetic path `content:<name>` without the prefix.
pub(crate) fn content_bundle_name(path: &str, name: Option<&str>) -> String {
    path.strip_prefix("content:")
        .or(name)
        .unwrap_or(path)
        .to_owned()
}

/// The deterministic `pg_cron` job-name prefix `pgokf.schedule_refresh`
/// fixes for a bundle's refresh job (`pgokf_refresh_<bundle_id>`); the web
/// crate reads jobs back by this convention, the extension owns it.
const REFRESH_JOB_PREFIX: &str = "pgokf_refresh_";

/// The `cron.job` read behind [`Db::refresh_schedules`], kept as a constant
/// so the scratch-database test executes exactly what ships.
const REFRESH_SCHEDULES_SQL: &str = "SELECT jobname, schedule FROM cron.job
     WHERE jobname LIKE $1 ESCAPE '\\' ORDER BY jobname";

/// The LIKE pattern of the job-name convention (its underscores escaped,
/// so they cannot act as wildcards).
fn refresh_job_pattern() -> String {
    REFRESH_JOB_PREFIX.replace('_', "\\_") + "%"
}

/// One `cron.job` row as a bundle's refresh schedule, or `None` when the
/// job name is not the convention's `pgokf_refresh_<integer>` shape.
fn refresh_schedule_of(jobname: &str, schedule: String) -> Option<RefreshSchedule> {
    let bundle_id = jobname.strip_prefix(REFRESH_JOB_PREFIX)?.parse().ok()?;
    Some(RefreshSchedule {
        bundle_id,
        schedule,
    })
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

/// A bundle as the admin page lists it: every row, retired ones included.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct AdminBundle {
    pub id: i64,
    pub path: String,
    pub name: String,
    pub source_type: String,
    pub enabled: bool,
    pub retired: bool,
    pub file_count: i32,
    pub last_synced_at: Option<String>,
    /// The producer-attested freshness state (`pgokf.effective_freshness`,
    /// bundle scope); `None` when no freshness row exists.
    pub freshness: Option<String>,
}

/// A bundle's scheduled content refresh as `pg_cron` holds it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RefreshSchedule {
    pub bundle_id: i64,
    /// The cron expression or interval phrase the job runs on.
    pub schedule: String,
}

/// A concept a person produced or verified, for their profile.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct PersonalItem {
    pub bundle_id: i64,
    pub bundle_name: String,
    pub concept_id: String,
    pub path: String,
    pub title: Option<String>,
    pub concept_type: Option<String>,
    pub trust_tier: String,
    pub when: Option<String>,
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
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct Facet {
    pub value: String,
    pub count: i64,
}

/// The filters a search carries; an empty `concept_types` with no
/// `type_group` means "no type filter" for every field.
#[derive(Debug, Clone, Default)]
pub(crate) struct SearchQuery {
    pub query: String,
    pub bundle_id: Option<i64>,
    /// The exact types to match. Several come from expanding a selected
    /// type group; `pgokf.concept_search` takes one exact type, so a
    /// multi-type query unions one bounded call per type in a single
    /// statement the database merges.
    pub concept_types: Vec<String>,
    /// The selected type-group slug, kept so a group that expands to no
    /// observed member reads as "matches nothing", never as "no filter".
    pub type_group: Option<String>,
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
            || !self.concept_types.is_empty()
            || self.type_group.is_some()
            || !self.tags.is_empty()
            || self.status.is_some()
            || self.trust_tier.is_some()
    }

    /// `true` when a type group was selected but expanded to no observed
    /// member: every typed query must come back empty.
    pub(crate) fn type_filter_impossible(&self) -> bool {
        self.type_group.is_some() && self.concept_types.is_empty()
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
/// catalog-wide graph; `degree` is the number of drawn edges it takes part
/// in (per the selected [`EdgeSource`]).
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

/// Which edges a graph picture draws: authored Markdown links, typed
/// relationships, or both.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EdgeSource {
    Links,
    Relationships,
    Both,
}

impl EdgeSource {
    /// The query-string value (`edges=<value>`).
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Links => "links",
            Self::Relationships => "rels",
            Self::Both => "both",
        }
    }

    pub(crate) fn includes_links(self) -> bool {
        matches!(self, Self::Links | Self::Both)
    }

    pub(crate) fn includes_relationships(self) -> bool {
        matches!(self, Self::Relationships | Self::Both)
    }
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

/// A relation type folded into a typed edge, with its own direction: one
/// pair's fold can mix directed and undirected rows, and every inspection
/// surface renders each type with the direction it actually has.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct RelationType {
    pub name: String,
    pub undirected: bool,
}

/// A typed edge between two graph nodes, possibly across bundles, folded
/// over parallel relationship rows and keeping the distinct relation types,
/// each with its own direction. `undirected` is set only when every folded
/// row is undirected, so the client draws an arrow whenever at least one
/// directed row remains.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct GraphRelationship {
    pub source_bundle_id: i64,
    pub source: String,
    pub target_bundle_id: i64,
    pub target: String,
    pub count: i64,
    pub relations: Vec<RelationType>,
    pub undirected: bool,
}

/// A graph picture as nodes and edges.
#[derive(Debug, Clone, Default, Serialize)]
pub(crate) struct Graph {
    pub nodes: Vec<GraphNode>,
    pub links: Vec<GraphLink>,
    pub relationships: Vec<GraphRelationship>,
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
                // 55P03 lock_not_available: another writer holds the bundle
                // and this one waited its bounded turn. Retryable, and a
                // server fault only if it keeps happening.
                "55P03" => Failure::Busy,
                _ if code.starts_with("22") => Failure::InvalidInput,
                _ => Failure::Other,
            };
        }
    }
    Failure::Other
}

/// The `SQLSTATE` of the first `PostgreSQL` error in the chain, if any.
pub(crate) fn sql_state(error: &anyhow::Error) -> Option<tokio_postgres::error::SqlState> {
    error
        .chain()
        .filter_map(|cause| cause.downcast_ref::<tokio_postgres::Error>())
        .find_map(tokio_postgres::Error::code)
        .cloned()
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

pub(crate) fn iso(column: &str) -> String {
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

    /// Run one statement and return the rows it affected. Like every helper
    /// here, the connection goes back to the pool whether the statement
    /// succeeded or failed - a failed statement is not a broken connection.
    pub(crate) async fn execute(&self, sql: &str, params: &[&(dyn ToSql + Sync)]) -> Result<u64> {
        let mut client = self.client().await?;
        let affected = client
            .get()
            .execute(sql, params)
            .await
            .context("catalog statement failed");
        client.finish();
        affected
    }

    /// Run a query that must return exactly one row.
    pub(crate) async fn query_one(&self, sql: &str, params: &[&(dyn ToSql + Sync)]) -> Result<Row> {
        self.query_opt(sql, params)
            .await?
            .context("the catalog returned no row where one was expected")
    }

    pub(crate) async fn query(
        &self,
        sql: &str,
        params: &[&(dyn ToSql + Sync)],
    ) -> Result<Vec<Row>> {
        let mut client = self.client().await?;
        let rows = client
            .get()
            .query(sql, params)
            .await
            .context("catalog query failed");
        client.finish();
        rows
    }

    pub(crate) async fn query_opt(
        &self,
        sql: &str,
        params: &[&(dyn ToSql + Sync)],
    ) -> Result<Option<Row>> {
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

    /// Whether `name` is this session's name for bundle `id` alone.
    ///
    /// `register_bundle_content` resolves a content bundle by **name**
    /// within the session's own tenant, which is not necessarily the row a
    /// lookup by id found: a session not scoped to a tenant sees every
    /// tenant's bundles, so writing one back by name could create or
    /// replace a different tenant's. Proven before the write, and the
    /// outcome's bundle id is checked against it after.
    ///
    /// # Errors
    ///
    /// The catalog cannot be read.
    pub(crate) async fn content_name_is_only(&self, name: &str, id: i64) -> Result<bool> {
        let row = self
            .query_one(
                "SELECT count(*), min(b.id)
                 FROM pgokf.bundles b
                 WHERE b.name = $1 AND b.source_type = 'content'
                   AND b.tenant_id
                       = coalesce(nullif(current_setting('pgokf.tenant', true), ''), 'default')",
                &[&name],
            )
            .await?;
        let seen: i64 = col(&row, 0)?;
        let only: Option<i64> = col(&row, 1)?;
        Ok(seen == 1 && only == Some(id))
    }

    /// What a content bundle carries whose bytes the catalog does not keep,
    /// if anything: an `index.md` (its `okf_version`) or a `log.md` (its
    /// changelog). Neither is a concept, so a full-snapshot rewrite - which
    /// is how one document is changed in a content bundle - would drop them.
    ///
    /// # Errors
    ///
    /// The catalog cannot be read.
    pub(crate) async fn bundle_carries_unstored(&self, bundle_id: i64) -> Result<Option<String>> {
        let row = self
            .query_one(
                "SELECT b.okf_version IS NOT NULL,
                        EXISTS (SELECT 1 FROM pgokf.bundle_log l WHERE l.bundle_id = b.id)
                 FROM pgokf.bundles b WHERE b.id = $1",
                &[&bundle_id],
            )
            .await?;
        let has_index: bool = col(&row, 0)?;
        let has_log: bool = col(&row, 1)?;
        Ok(match (has_index, has_log) {
            (false, false) => None,
            (true, true) => Some("an index.md and a log.md".to_owned()),
            (true, false) => Some("an index.md".to_owned()),
            _ => Some("a log.md".to_owned()),
        })
    }

    /// Take the lock every writer of this content bundle takes, on a
    /// connection held for as long as the change runs, and released with it.
    ///
    /// A content change is a read of the whole bundle followed by a write of
    /// the whole bundle, so two of them interleaving loses one. A lock in
    /// this process is not enough: `pgokf-mcp` writes into the same bundles,
    /// and so does a second instance of this UI. This is the lock they all
    /// take, keyed on the bundle's name, which is what
    /// `register_bundle_content` addresses.
    ///
    /// # Errors
    ///
    /// The catalog cannot be reached.
    pub(crate) async fn lock_content_bundle(&self, name: &str) -> Result<ContentLock> {
        let held = self.checkout().await?;
        // Bounded: a writer that has wedged holding this lock must not stop
        // every other writer for ever.
        held.client()
            .execute(
                "SELECT set_config('lock_timeout', $1, false)",
                &[&LOCK_WAIT],
            )
            .await
            .context("bounding the wait for the bundle's other writers")?;
        held.client()
            .execute(
                "SELECT pg_advisory_lock(hashtext('pgokf.content_bundle'), hashtext($1))",
                &[&name],
            )
            .await
            .context(
                "waiting for the bundle's other writers (another writer is changing this \
                 bundle; try again)",
            )?;
        Ok(ContentLock {
            held,
            name: name.to_owned(),
        })
    }

    /// Whether a content bundle of this name already exists in this
    /// instance's own tenant, in any state - which is the name a creation
    /// here would take, and the only one it could collide with.
    ///
    /// The extension keys a content bundle on the synthetic path
    /// `content:<name>`, so a disabled or retired one still collides even
    /// though [`Db::content_bundles`] hides it - and a collision on the
    /// "new bundle" path would resync the existing bundle to just the files
    /// in hand, deleting the rest.
    ///
    /// # Errors
    ///
    /// A catalog query failure.
    pub(crate) async fn content_bundle_exists(&self, name: &str) -> Result<bool> {
        let path = format!("content:{name}");
        Ok(self
            .query_opt("SELECT 1 FROM pgokf.bundles WHERE path = $1", &[&path])
            .await?
            .is_some())
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

    /// Every bundle for the admin page, retired ones included, each with its
    /// producer-attested freshness state (the bundle-scope row of the
    /// reader-granted `pgokf.effective_freshness` projection).
    pub(crate) async fn admin_bundles(&self) -> Result<Vec<AdminBundle>> {
        let sql = format!(
            "SELECT b.id, b.path, {}, b.source_type, b.enabled, (b.retired_at IS NOT NULL),
                    b.file_count, {}, f.state
             FROM pgokf.bundles b
             LEFT JOIN pgokf.effective_freshness f
                    ON f.bundle_id = b.id AND f.scope_kind = 'bundle'
             ORDER BY (b.retired_at IS NOT NULL), b.id",
            display_name("b"),
            iso("b.last_synced_at")
        );
        self.query_map(&sql, &[], |r| {
            Ok(AdminBundle {
                id: col(r, 0)?,
                path: col(r, 1)?,
                name: col(r, 2)?,
                source_type: col(r, 3)?,
                enabled: col(r, 4)?,
                retired: col(r, 5)?,
                file_count: col(r, 6)?,
                last_synced_at: col(r, 7)?,
                freshness: col(r, 8)?,
            })
        })
        .await
    }

    /// A bundle's source type and path as the database sees it.
    pub(crate) async fn bundle_source(&self, id: i64) -> Result<Option<(String, String)>> {
        let row = self
            .query_opt(
                "SELECT source_type, path FROM pgokf.bundles
                 WHERE id = $1 AND enabled AND retired_at IS NULL",
                &[&id],
            )
            .await?;
        row.map(|r| Ok((col(&r, 0)?, col(&r, 1)?))).transpose()
    }

    /// Run one writer-tier bundle function that returns `bundle_info`.
    async fn bundle_op(&self, sql: &str, params: &[&(dyn ToSql + Sync)]) -> Result<()> {
        self.query_opt(sql, params)
            .await?
            .ok_or_else(|| anyhow!("the bundle operation returned no row"))?;
        Ok(())
    }

    pub(crate) async fn refresh_bundle(&self, id: i64) -> Result<SyncOutcome> {
        let row = self
            .query_opt(
                "SELECT r.bundle_id, r.added, r.updated, r.removed FROM pgokf.refresh_bundle($1) r",
                &[&id],
            )
            .await?
            .ok_or_else(|| anyhow!("refresh_bundle returned no row"))?;
        Ok(SyncOutcome {
            bundle_id: col(&row, 0)?,
            added: col::<Option<i32>>(&row, 1)?.unwrap_or_default(),
            updated: col::<Option<i32>>(&row, 2)?.unwrap_or_default(),
            removed: col::<Option<i32>>(&row, 3)?.unwrap_or_default(),
        })
    }

    pub(crate) async fn set_bundle_enabled(&self, id: i64, enabled: bool) -> Result<()> {
        self.bundle_op(
            "SELECT * FROM pgokf.set_bundle_enabled($1, $2)",
            &[&id, &enabled],
        )
        .await
    }

    pub(crate) async fn retire_bundle(&self, id: i64, retire: bool) -> Result<()> {
        if retire {
            self.bundle_op("SELECT * FROM pgokf.retire_bundle($1)", &[&id])
                .await
        } else {
            self.bundle_op("SELECT * FROM pgokf.unretire_bundle($1)", &[&id])
                .await
        }
    }

    pub(crate) async fn unregister_bundle(&self, id: i64) -> Result<()> {
        self.bundle_op("SELECT * FROM pgokf.unregister_bundle($1)", &[&id])
            .await
    }

    /// Register (or re-schedule, idempotently) a recurring content refresh
    /// for a bundle (`pgokf.schedule_refresh`; the `pg_cron` adapter).
    /// Returns the deterministic job name. The function is admin-tier
    /// (`pgokf_admin`) and raises `22023` when `pg_cron` is absent or the
    /// schedule is malformed.
    pub(crate) async fn schedule_refresh(&self, id: i64, schedule: &str) -> Result<String> {
        let row = self
            .query_opt("SELECT pgokf.schedule_refresh($1, $2)", &[&id, &schedule])
            .await?
            .ok_or_else(|| anyhow!("schedule_refresh returned no row"))?;
        col(&row, 0)
    }

    /// Remove a bundle's scheduled refresh (`pgokf.unschedule_refresh`):
    /// `true` when a job was removed, `false` for a clean no-op (no
    /// `pg_cron`, or no such job).
    pub(crate) async fn unschedule_refresh(&self, id: i64) -> Result<bool> {
        let row = self
            .query_opt("SELECT pgokf.unschedule_refresh($1)", &[&id])
            .await?
            .ok_or_else(|| anyhow!("unschedule_refresh returned no row"))?;
        col(&row, 0)
    }

    /// Every scheduled content refresh, keyed by bundle.
    ///
    /// The extension has no reader surface that lists scheduled refreshes
    /// (`crates/extension/src/catalog/schedule.rs` installs only the two
    /// mutators), so this reads `pg_cron`'s own job table and matches the
    /// deterministic `pgokf_refresh_<bundle_id>` job-name convention the
    /// extension fixes. Role semantics decide where it may run: `pg_cron`
    /// grants `SELECT` on `cron.job` to nobody by default, so the pooled
    /// reader role can never answer this and the query must go over the
    /// app's writer connection, whose login role the operator grants
    /// `USAGE` on schema `cron` and `SELECT` on `cron.job`. Callers treat
    /// `42P01` (no `pg_cron` in this database) and `42501` (the grant is
    /// missing) as "schedules not visible", not as page failures.
    pub(crate) async fn refresh_schedules(&self) -> Result<Vec<RefreshSchedule>> {
        let rows = self
            .query_map(REFRESH_SCHEDULES_SQL, &[&refresh_job_pattern()], |r| {
                Ok((col::<String>(r, 0)?, col::<String>(r, 1)?))
            })
            .await?;
        Ok(rows
            .into_iter()
            .filter_map(|(jobname, schedule)| refresh_schedule_of(&jobname, schedule))
            .collect())
    }

    /// Register a directory bundle at a path the database server can read.
    pub(crate) async fn register_bundle(
        &self,
        path: &str,
        name: Option<&str>,
    ) -> Result<SyncOutcome> {
        let row = self
            .query_opt(
                "SELECT r.bundle_id, r.added, r.updated, r.removed
                 FROM pgokf.register_bundle($1, $2, '{}'::jsonb) r",
                &[&path, &name],
            )
            .await?
            .ok_or_else(|| anyhow!("register_bundle returned no row"))?;
        Ok(SyncOutcome {
            bundle_id: col(&row, 0)?,
            added: col::<Option<i32>>(&row, 1)?.unwrap_or_default(),
            updated: col::<Option<i32>>(&row, 2)?.unwrap_or_default(),
            removed: col::<Option<i32>>(&row, 3)?.unwrap_or_default(),
        })
    }

    /// Concepts whose current content a person produced (`generated.by`).
    pub(crate) async fn produced_by(&self, actor: &str, limit: i64) -> Result<Vec<PersonalItem>> {
        let sql = format!(
            "SELECT c.bundle_id, {}, c.id, c.path, c.title, c.type,
                    coalesce(p.trust_tier, 'unverified'), {}
             FROM pgokf.concept_provenance p
             JOIN pgokf.concepts c ON c.bundle_id = p.bundle_id AND c.id = p.concept_id
             JOIN pgokf.bundles b ON b.id = c.bundle_id AND b.enabled AND b.retired_at IS NULL
             WHERE p.generated_by = $1
             ORDER BY p.generated_at DESC NULLS LAST, c.path
             LIMIT $2",
            display_name("b"),
            iso("p.generated_at")
        );
        self.query_map(&sql, &[&actor, &limit], personal_item).await
    }

    /// Concepts a person verified.
    pub(crate) async fn verified_by(&self, actor: &str, limit: i64) -> Result<Vec<PersonalItem>> {
        let sql = format!(
            "SELECT DISTINCT ON (c.bundle_id, c.id) c.bundle_id, {}, c.id, c.path, c.title, c.type,
                    coalesce(p.trust_tier, 'unverified'), {}
             FROM pgokf.concept_verification v
             JOIN pgokf.concepts c ON c.bundle_id = v.bundle_id AND c.id = v.concept_id
             JOIN pgokf.bundles b ON b.id = c.bundle_id AND b.enabled AND b.retired_at IS NULL
             LEFT JOIN pgokf.concept_provenance p
                    ON p.bundle_id = c.bundle_id AND p.concept_id = c.id
             WHERE v.verified_by = $1
             ORDER BY c.bundle_id, c.id, v.verified_at DESC NULLS LAST
             LIMIT $2",
            display_name("b"),
            iso("v.verified_at")
        );
        self.query_map(&sql, &[&actor, &limit], personal_item).await
    }

    /// Up to `limit` concepts of one bundle ordered by path, starting after
    /// `after_path` (keyset paging for large bundles).
    pub(crate) async fn bundle_concepts(
        &self,
        bundle_id: i64,
        after_path: Option<&str>,
        limit: i64,
    ) -> Result<Vec<ConceptSummary>> {
        // The same bundle guard every other concept read carries, so a
        // disabled or retired bundle's listing matches what opening one does
        // (404): no dead rows, and no metadata leak from a hidden bundle.
        let sql = format!(
            "SELECT c.bundle_id, c.id, c.path, c.type, c.title, c.description,
                    coalesce(c.tags, '{{}}'), {}
             FROM pgokf.concepts c
             JOIN pgokf.bundles b ON b.id = c.bundle_id AND b.enabled AND b.retired_at IS NULL
             WHERE c.bundle_id = $1 AND ($2::text IS NULL OR c.path > $2)
             ORDER BY c.path LIMIT $3",
            iso("c.modified_at")
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
        if q.type_filter_impossible() {
            return Ok(Vec::new());
        }
        let sql = format!(
            "SELECT c.bundle_id, c.id, c.path, c.type, c.title, c.description,
                    coalesce(c.tags, '{{}}'), {}
             FROM pgokf.concepts c
             JOIN pgokf.bundles b
                    ON b.id = c.bundle_id AND b.enabled AND b.retired_at IS NULL
             LEFT JOIN pgokf.concept_provenance p
                    ON p.bundle_id = c.bundle_id AND p.concept_id = c.id
             WHERE ($1::bigint IS NULL OR c.bundle_id = $1)
               AND (cardinality($2::text[]) = 0 OR c.type = ANY($2))
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
                &q.concept_types,
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
    /// `concept_search` takes one exact type, so a multi-type query (an
    /// expanded type group) unions one bounded call per type in a single
    /// statement whose `ORDER BY` and final `LIMIT` run in the database:
    /// the merged order and truncation then live under the same database
    /// collation as the per-type streams and their keyset cursor
    /// predicates. (A Rust-side merge compares concept ids by bytes, which
    /// a non-C collation orders differently, and tied hits can page-skip.)
    /// The keyset cursor is a global cutoff, so passing it to every
    /// per-type call keeps the merged pages gapless.
    pub(crate) async fn search(&self, q: &SearchQuery, limit: i32) -> Result<Vec<Hit>> {
        if q.type_filter_impossible() {
            return Ok(Vec::new());
        }
        let [first, rest @ ..] = q.concept_types.as_slice() else {
            return self.search_one_type(q, None, limit).await;
        };
        if rest.is_empty() {
            return self.search_one_type(q, Some(first), limit).await;
        }
        let cursor = q.after.as_ref().map(|c| {
            serde_json::json!({"rank": c.rank, "bundle_id": c.bundle_id, "concept_id": c.concept_id})
        });
        let tags = q.tags_param();
        let mut params: Vec<&(dyn ToSql + Sync)> = vec![&q.query, &q.bundle_id, &limit];
        for concept_type in &q.concept_types {
            params.push(concept_type);
        }
        params.push(&tags);
        params.push(&q.status);
        params.push(&q.trust_tier);
        params.push(&cursor);
        self.query_map(&search_many_types_sql(q.concept_types.len()), &params, hit)
            .await
    }

    /// One [`SearchQuery::search`] stream: a single exact type (or none).
    async fn search_one_type(
        &self,
        q: &SearchQuery,
        concept_type: Option<&str>,
        limit: i32,
    ) -> Result<Vec<Hit>> {
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
                &concept_type,
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
    /// search with a caller-supplied query embedding. The extension functions
    /// take the type filter as their trailing `concept_types` argument and
    /// apply it inside the ranked query, before the candidate list is
    /// truncated to the page size (on both fusion inputs for hybrid), so a
    /// filtered page is exactly the type-filtered top-`limit` and can never
    /// come back empty or underfilled while eligible hits of the selected
    /// types exist. A group that expanded to nothing short-circuits to no
    /// rows in every mode, before any statement runs.
    pub(crate) async fn search_with_embedding(
        &self,
        q: &SearchQuery,
        embedding: &[f32],
        hybrid: bool,
    ) -> Result<Vec<Hit>> {
        if q.type_filter_impossible() {
            return Ok(Vec::new());
        }
        let embedding: Vec<f32> = embedding.to_vec();
        let limit = q.limit;
        if hybrid {
            self.query_map(
                embedding_search_sql(true),
                &[&q.query, &embedding, &q.bundle_id, &limit, &q.concept_types],
                hit,
            )
            .await
        } else {
            self.query_map(
                embedding_search_sql(false),
                &[&embedding, &q.bundle_id, &limit, &q.concept_types],
                hit,
            )
            .await
        }
    }

    /// Every distinct concept type in the visible catalog (optionally one
    /// bundle), unbounded: the membership inventory type-group expansion
    /// uses. Kept separate from the display facets, whose top-100 cap is a
    /// presentation bound - expanding a group against a capped list would
    /// drop every type beyond the cap and could make a selected group
    /// falsely match nothing.
    pub(crate) async fn catalog_types(&self, bundle_id: Option<i64>) -> Result<Vec<String>> {
        self.query_map(CATALOG_TYPES_SQL, &[&bundle_id], |r| col(r, 0))
            .await
    }

    /// `pgokf.search_facets` for one facet name (`type`, `bundle`, `tag`,
    /// `status`, `trust_tier`) over the query's matches. A multi-type query
    /// sums one bucket list per type, as [`SearchQuery::search`] does.
    pub(crate) async fn facets(&self, q: &SearchQuery, facet: &str) -> Result<Vec<Facet>> {
        if q.type_filter_impossible() {
            return Ok(Vec::new());
        }
        let [first, rest @ ..] = q.concept_types.as_slice() else {
            return self.facets_one_type(q, None, facet).await;
        };
        if rest.is_empty() {
            return self.facets_one_type(q, Some(first), facet).await;
        }
        let mut streams = Vec::with_capacity(q.concept_types.len());
        for concept_type in &q.concept_types {
            streams.push(self.facets_one_type(q, Some(concept_type), facet).await?);
        }
        Ok(merge_facets(streams))
    }

    /// One [`SearchQuery::facets`] stream: a single exact type (or none).
    async fn facets_one_type(
        &self,
        q: &SearchQuery,
        concept_type: Option<&str>,
        facet: &str,
    ) -> Result<Vec<Facet>> {
        self.query_map(
            "SELECT facet_value, count
             FROM pgokf.search_facets($1, $2, $3, $4, $5, $6, $7)
             ORDER BY count DESC, facet_value",
            &[
                &q.query,
                &q.bundle_id,
                &facet,
                &concept_type,
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
             JOIN pgokf.bundles b ON b.id = c.bundle_id AND b.enabled AND b.retired_at IS NULL
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
    /// `pgokf.concept_neighbors()` reaches within `max_hops`, and the edges
    /// the selected sources draw among that set (the node set itself is the
    /// link neighborhood either way). Empty when the seed is not visible.
    pub(crate) async fn graph(
        &self,
        bundle_id: i64,
        concept_id: &str,
        max_hops: i32,
        edges: EdgeSource,
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
        self.edges_among(nodes, total, edges).await
    }

    /// The catalog-wide graph: the `limit` best-connected visible concepts
    /// (of the selected bundles and concept types, or all when either list
    /// is empty) and the edges the selected sources draw among them. The
    /// degree that ranks and cuts the nodes counts exactly the edges the
    /// selected sources draw, from one aggregate, not a probe per concept.
    pub(crate) async fn catalog_graph(
        &self,
        bundle_ids: &[i64],
        types: &[String],
        limit: i64,
        edges: EdgeSource,
    ) -> Result<Graph> {
        let rows = self
            .query(&catalog_graph_sql(edges), &[&bundle_ids, &limit, &types])
            .await?;
        let total = rows
            .first()
            .map(|r| col::<i64>(r, 8))
            .transpose()?
            .unwrap_or(0);
        let nodes = rows.iter().map(graph_node).collect::<Result<Vec<_>>>()?;
        self.edges_among(nodes, total, edges).await
    }

    /// The drawn edges of the selected sources among `nodes`.
    async fn edges_among(
        &self,
        nodes: Vec<GraphNode>,
        total: i64,
        edges: EdgeSource,
    ) -> Result<Graph> {
        let links = if edges.includes_links() {
            self.links_among(&nodes).await?
        } else {
            Vec::new()
        };
        let relationships = if edges.includes_relationships() {
            self.relationships_among(&nodes).await?
        } else {
            Vec::new()
        };
        Ok(Graph {
            nodes,
            links,
            relationships,
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

    /// The resolved typed relationships whose both ends are in `nodes`
    /// (self-pairs excluded, as with links), folded per endpoint pair. Reads
    /// `pgokf.current_relationships` - the reader-granted projection, so
    /// active-publication filtering, tenant scoping, and target-bundle
    /// visibility apply exactly as the extension defines them; the raw
    /// tables stay untouched, like every other reader query here.
    async fn relationships_among(&self, nodes: &[GraphNode]) -> Result<Vec<GraphRelationship>> {
        if nodes.is_empty() {
            return Ok(Vec::new());
        }
        let bundles: Vec<i64> = nodes.iter().map(|n| n.bundle_id).collect();
        let ids: Vec<&str> = nodes.iter().map(|n| n.id.as_str()).collect();
        self.query_map(RELATIONSHIPS_AMONG_SQL, &[&bundles, &ids], |r| {
            // A resolved row always carries both target halves (the table
            // CHECK guarantees it); the options are defensive, not load-bearing.
            let Some(target_bundle_id) = col::<Option<i64>>(r, 2)? else {
                return Ok(None);
            };
            let Some(target) = col::<Option<String>>(r, 3)? else {
                return Ok(None);
            };
            Ok(Some(GraphRelationship {
                source_bundle_id: col(r, 0)?,
                source: col(r, 1)?,
                target_bundle_id,
                target,
                count: col(r, 4)?,
                // The fold's `[name, undirected]` jsonb pairs, in relation
                // type order.
                relations: serde_json::from_value::<Vec<(String, bool)>>(col(r, 5)?)?
                    .into_iter()
                    .map(|(name, undirected)| RelationType { name, undirected })
                    .collect(),
                undirected: col::<Option<bool>>(r, 6)?.unwrap_or(false),
            }))
        })
        .await
        .map(|rows| rows.into_iter().flatten().collect())
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
                 JOIN pgokf.bundles b ON b.id = c.bundle_id AND b.enabled AND b.retired_at IS NULL
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

/// The lock a content change holds against every other writer of the same
/// bundle, in this process or another.
///
/// [`ContentLock::release`] unlocks and returns the connection to the pool.
/// Dropping it without that is safe but wasteful: the connection is closed,
/// which is what releases the session lock.
pub(crate) struct ContentLock {
    held: Borrowed,
    name: String,
}

impl ContentLock {
    /// Let the next writer of this bundle in, and give the connection back.
    ///
    /// Dropping the lock without this is safe - the connection closes and
    /// `PostgreSQL` releases a session lock with its session - it merely costs
    /// the pool a connection, so the callers release explicitly.
    pub(crate) async fn release(mut self) {
        // The bound was set on the session, and the pool hands this
        // connection on as it is, so it is put back as it was found.
        let released = self
            .held
            .client()
            .execute(
                "SELECT pg_advisory_unlock(hashtext('pgokf.content_bundle'), hashtext($1))",
                &[&self.name],
            )
            .await;
        // Back to whatever this server configures, which is not necessarily
        // no bound at all, so the next borrower of this connection inherits
        // neither this one's wait nor a wrong default.
        let restored = self.held.client().batch_execute("RESET lock_timeout").await;
        if released.is_ok() && restored.is_ok() {
            self.held.finish();
        }
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

    /// The underlying client mutably, for a caller that opens a transaction
    /// (the plugin builder's one-snapshot build). A holder that finishes
    /// with a transaction still open is dropped, not returned to the pool:
    /// `finish` is for work that left the connection in autocommit.
    pub(crate) fn client_mut(&mut self) -> &mut tokio_postgres::Client {
        use std::ops::DerefMut as _;
        self.object
            .as_mut()
            .expect("a borrowed connection is present until finish or drop")
            .deref_mut()
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

/// `catalog_graph`'s query. `$1` is the selected bundle ids as `bigint[]`
/// and `$3` the selected concept types as `text[]`; an empty array means
/// every bundle (or type), so the membership test collapses to true. The
/// type constraint lives in the `visible` CTE only: degree still counts
/// every drawn edge, matching how an unfiltered graph ranks a node.
///
/// The `ends` CTE counts exactly the edges the selected [`EdgeSource`]
/// draws: `Links` reproduces the pre-relationships graph byte for byte,
/// `Relationships` ranks by typed edges alone, and `Both` sums the two -
/// "best connected" always means best connected in the picture being drawn.
/// Relationship ends come from `pgokf.current_relationships`, the
/// reader-granted projection, so the degree never counts an edge the same
/// session could not draw.
fn catalog_graph_sql(edges: EdgeSource) -> String {
    let link_ends = "SELECT l.bundle_id, l.source_id AS id FROM pgokf.links l
              WHERE l.resolved AND l.source_id <> l.target_id
                AND (cardinality($1::bigint[]) = 0 OR l.bundle_id = ANY($1))
             UNION ALL
             SELECT l.bundle_id, l.target_id FROM pgokf.links l
              WHERE l.resolved AND l.source_id <> l.target_id
                AND (cardinality($1::bigint[]) = 0 OR l.bundle_id = ANY($1))";
    // The aliases name the ends CTE's columns: a UNION branch's output
    // columns take the first SELECT's names, so without them the `deg` CTE's
    // `bundle_id, id` references would not resolve.
    let relationship_ends =
        "SELECT r.source_bundle_id AS bundle_id, r.source_concept_id AS id FROM pgokf.current_relationships r
              WHERE NOT r.unresolved
                AND NOT (r.source_bundle_id = r.target_bundle_id
                         AND r.source_concept_id = r.target_concept_id)
                AND (cardinality($1::bigint[]) = 0 OR r.source_bundle_id = ANY($1))
             UNION ALL
             SELECT r.target_bundle_id, r.target_concept_id FROM pgokf.current_relationships r
              WHERE NOT r.unresolved
                AND NOT (r.source_bundle_id = r.target_bundle_id
                         AND r.source_concept_id = r.target_concept_id)
                AND (cardinality($1::bigint[]) = 0 OR r.target_bundle_id = ANY($1))";
    let ends = match edges {
        EdgeSource::Links => link_ends.to_owned(),
        EdgeSource::Relationships => relationship_ends.to_owned(),
        EdgeSource::Both => {
            format!("{link_ends}\n             UNION ALL\n             {relationship_ends}")
        }
    };
    format!(
        "WITH ends AS (
             {ends}
         ), deg AS (
             SELECT bundle_id, id, count(*)::bigint AS degree FROM ends GROUP BY 1, 2
         ), visible AS (
             SELECT c.bundle_id, c.id, c.title, c.type, c.path, {} AS bundle_name,
                    coalesce(deg.degree, 0) AS degree
             FROM pgokf.concepts c
             JOIN pgokf.bundles b ON b.id = c.bundle_id AND b.enabled AND b.retired_at IS NULL
             LEFT JOIN deg ON deg.bundle_id = c.bundle_id AND deg.id = c.id
             WHERE (cardinality($1::bigint[]) = 0 OR c.bundle_id = ANY($1))
               AND (cardinality($3::text[]) = 0 OR c.type = ANY($3))
         )
         SELECT bundle_id, bundle_name, id, title, type, path, 0::int, degree,
                count(*) OVER ()
         FROM visible
         ORDER BY degree DESC, bundle_id, id
         LIMIT $2",
        display_name("b")
    )
}

/// `relationships_among`'s query: the resolved typed edges of
/// `pgokf.current_relationships` whose both ends are drawn nodes (`$1` the
/// node bundle ids, `$2` the concept ids, pairwise). Self-pairs are
/// excluded (a self-edge would render as nothing, matching the links side);
/// unresolved and external rows never join a drawn node by construction and
/// are excluded explicitly for clarity. The fold is two-level: rows first
/// collapse per relation type and direction, then per endpoint pair, so the
/// payload keeps each distinct relation type *with its own direction* (one
/// `[name, undirected]` pair per type) - a mixed pair can show which member
/// types are undirected, which a flat type list with one bool could not.
/// The edge as a whole is undirected only when every folded row is.
const RELATIONSHIPS_AMONG_SQL: &str = "
    WITH n AS (SELECT * FROM ROWS FROM (unnest($1::bigint[]), unnest($2::text[])) AS t(bundle_id, id)),
         per_type AS (
             SELECT r.source_bundle_id, r.source_concept_id, r.target_bundle_id, r.target_concept_id,
                    r.relation_type, r.direction = 'undirected' AS undirected, count(*) AS rows
             FROM pgokf.current_relationships r
             JOIN n s ON s.bundle_id = r.source_bundle_id AND s.id = r.source_concept_id
             JOIN n t ON t.bundle_id = r.target_bundle_id AND t.id = r.target_concept_id
             WHERE NOT r.unresolved
               AND NOT (r.source_bundle_id = r.target_bundle_id
                        AND r.source_concept_id = r.target_concept_id)
             GROUP BY 1, 2, 3, 4, 5, 6
         )
    SELECT source_bundle_id, source_concept_id, target_bundle_id, target_concept_id,
           sum(rows)::bigint,
           jsonb_agg(jsonb_build_array(relation_type, undirected) ORDER BY relation_type),
           bool_and(undirected)
    FROM per_type
    GROUP BY 1, 2, 3, 4 ORDER BY 1, 2, 3, 4";

/// The single statement of a multi-type search: one bounded
/// `concept_search` call per exact type (`$4` through `$3 + type_count`),
/// unioned, with the merge - the rank/bundle/id total order and the final
/// truncation - computed by the database. One collation then governs the
/// per-type streams, the merged order, and the keyset cursor predicates
/// alike; merging in Rust would compare concept ids by bytes instead,
/// which diverges from the database's text collation (e.g. `en_US.UTF-8`
/// orders `a` before `B`, bytes the reverse) and lets equal-rank hits skip
/// a page boundary. The shared parameters are bound once and referenced
/// from every branch: `$1` query, `$2` bundle, `$3` limit (per call and,
/// reused, the final `LIMIT`), then after the types come tags, status,
/// trust tier, and the cursor.
fn search_many_types_sql(type_count: usize) -> String {
    let tags = 4 + type_count;
    let branch = |index: usize| {
        format!(
            "SELECT bundle_id, concept_id, path, title, type, rank, headline
             FROM pgokf.concept_search($1, $2, $3, ${}, ${tags}, ${}, ${}, ${})",
            4 + index,
            tags + 1,
            tags + 2,
            tags + 3
        )
    };
    let branches: Vec<String> = (0..type_count).map(branch).collect();
    format!(
        "{}
         ORDER BY rank DESC, bundle_id, concept_id
         LIMIT $3",
        branches.join("\nUNION ALL\n")
    )
}

/// Merge the per-type facet bucket lists of a multi-type search: counts sum
/// per bucket value, ordered by count descending then value, as one
/// `search_facets` call would return them.
fn merge_facets(streams: Vec<Vec<Facet>>) -> Vec<Facet> {
    let mut counts: BTreeMap<String, i64> = BTreeMap::new();
    for facet in streams.into_iter().flatten() {
        *counts.entry(facet.value).or_default() += facet.count;
    }
    let mut facets: Vec<Facet> = counts
        .into_iter()
        .map(|(value, count)| Facet { value, count })
        .collect();
    facets.sort_by(|a, b| b.count.cmp(&a.count).then_with(|| a.value.cmp(&b.value)));
    facets
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

/// The semantic/hybrid statement. The extension functions apply the
/// type-membership filter INSIDE the ranked query, before the candidate list
/// is truncated to the page size (the last parameter binds the expanded
/// exact types; `NULL` or empty is no filter), so a filtered page is exactly
/// the type-filtered top-`limit` - never a page underfilled by higher-ranked
/// excluded types.
fn embedding_search_sql(hybrid: bool) -> &'static str {
    if hybrid {
        "SELECT bundle_id, concept_id, path, title, type, rank, headline
         FROM pgokf.concept_search_hybrid($1, $2, $3, $4, $5)"
    } else {
        "SELECT bundle_id, concept_id, path, title, type, rank, headline
         FROM pgokf.concept_search_semantic($1, $2, $3, $4)"
    }
}

/// The complete visible-type inventory group membership expands against:
/// deliberately no `LIMIT`, unlike the top-100 display facets.
const CATALOG_TYPES_SQL: &str = "SELECT DISTINCT c.type
             FROM pgokf.concepts c
             JOIN pgokf.bundles b ON b.id = c.bundle_id AND b.enabled AND b.retired_at IS NULL
             WHERE ($1::bigint IS NULL OR c.bundle_id = $1)
             ORDER BY 1";

/// The union of both resource tables in one column shape (the nine columns
/// [`resource`] reads, then `bundle_id` for grouping).
// Metadata only: whether a reference has text, never the text itself.
// Reading content goes through the audited readers, so the access log has a
// row for it - selecting `text_body` here would have rendered a whole
// reference with no trace.
const RESOURCE_ROWS: &str = "SELECT r.concept_id, r.class, r.path, r.byte_size, r.sha256, r.detail,
                                    r.package_concept_id, r.textual, r.bundle_id
                             FROM (
                                 SELECT bundle_id, concept_id, 'script' AS class, source_path AS path,
                                        byte_size, executable_sha256 AS sha256, language AS detail,
                                        package_concept_id, true AS textual
                                 FROM pgokf.scripts
                                 UNION ALL
                                 SELECT bundle_id, concept_id,
                                        CASE WHEN source_path LIKE 'assets/%' THEN 'asset' ELSE 'reference' END,
                                        source_path, byte_size, content_sha256, media_type,
                                        package_concept_id, text_body IS NOT NULL
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

/// A pool to a port nothing listens on, for tests of what a catalog outage
/// does to a code path: every checkout fails, quickly, and nothing panics.
#[cfg(test)]
pub(crate) fn dead_db() -> Db {
    Db::connect(&DbConfig {
        database_url: "postgresql://nobody:nothing@127.0.0.1:9/okf",
        force_tls: false,
        pool_size: 1,
        tenant: None,
        statement_timeout_ms: 1_000,
    })
    .expect("a pool builds without a server")
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
    fn refresh_schedule_of_matches_the_job_name_convention_exactly() {
        // Arrange & Act & Assert: the prefix plus an integer id maps to a
        // bundle; lookalike names stay out.
        assert_eq!(
            refresh_schedule_of("pgokf_refresh_42", "0 * * * *".to_owned()),
            Some(RefreshSchedule {
                bundle_id: 42,
                schedule: "0 * * * *".to_owned(),
            })
        );
        assert_eq!(
            refresh_schedule_of("pgokf_refresh_7", "15 minutes".to_owned()).map(|s| s.bundle_id),
            Some(7)
        );
        assert!(refresh_schedule_of("pgokf_refresh_", "0 * * * *".to_owned()).is_none());
        assert!(refresh_schedule_of("pgokf_refresh_x", "0 * * * *".to_owned()).is_none());
        assert!(refresh_schedule_of("pgokf_refresh_1_extra", "0 * * * *".to_owned()).is_none());
        assert!(refresh_schedule_of("other_job", "0 * * * *".to_owned()).is_none());
    }

    #[test]
    fn the_refresh_job_pattern_escapes_the_conventions_underscores() {
        // Arrange & Act
        let pattern = refresh_job_pattern();

        // Assert: a LIKE wildcard `_` would let a lookalike job name through.
        assert_eq!(pattern, "pgokf\\_refresh\\_%");
    }

    #[tokio::test]
    async fn schedule_writes_and_the_schedule_read_issue_statements() {
        // Arrange / Act / Assert: against a dead pool every call errors,
        // proving each issues exactly the statement it wraps.
        let db = dead_db();
        assert!(db.schedule_refresh(7, "0 * * * *").await.is_err());
        assert!(db.unschedule_refresh(7).await.is_err());
        assert!(db.refresh_schedules().await.is_err());
    }

    /// The scratch database the schedule-read regression creates and drops.
    const SCHEDULE_SCRATCH_DB: &str = "pgokf_web_schedule_sql_test";

    /// The schedule read, executed for real over a stand-in `cron.job`
    /// (`pg_cron` itself is not needed): only `pgokf_refresh_<id>` jobs come
    /// back, keyed by their bundle id, and a lookalike name that LIKE's
    /// wildcard would have matched stays out.
    #[tokio::test]
    async fn refresh_schedules_sql_executes_against_a_scratch_database() {
        let Some((admin, mut config)) = scratch_admin().await else {
            return;
        };
        admin
            .batch_execute(&format!("DROP DATABASE IF EXISTS {SCHEDULE_SCRATCH_DB}"))
            .await
            .expect("drop a stale scratch database");
        admin
            .batch_execute(&format!("CREATE DATABASE {SCHEDULE_SCRATCH_DB}"))
            .await
            .expect("create the scratch database");
        let result = async {
            config.dbname(SCHEDULE_SCRATCH_DB);
            let (client, connection) = config
                .connect(NoTls)
                .await
                .context("connect to the scratch database")?;
            tokio::spawn(async move {
                if let Err(error) = connection.await {
                    eprintln!("scratch connection error: {error}");
                }
            });
            client
                .batch_execute(
                    "CREATE SCHEMA cron;
                     CREATE TABLE cron.job (jobname text PRIMARY KEY, schedule text NOT NULL);
                     INSERT INTO cron.job VALUES
                         ('pgokf_refresh_7', '0 * * * *'),
                         ('pgokf_refresh_3', '15 minutes'),
                         ('pgokfXrefreshX9', '0 0 * * *'),
                         ('nightly_backup', '0 3 * * *');",
                )
                .await?;
            let rows = client
                .query(REFRESH_SCHEDULES_SQL, &[&refresh_job_pattern()])
                .await
                .context("the schedule-read SQL executes")?;
            let schedules: Vec<RefreshSchedule> = rows
                .iter()
                .filter_map(|r| {
                    refresh_schedule_of(
                        &r.try_get::<_, String>(0).expect("jobname"),
                        r.try_get::<_, String>(1).expect("schedule"),
                    )
                })
                .collect();
            assert_eq!(
                schedules,
                vec![
                    RefreshSchedule {
                        bundle_id: 3,
                        schedule: "15 minutes".to_owned(),
                    },
                    RefreshSchedule {
                        bundle_id: 7,
                        schedule: "0 * * * *".to_owned(),
                    },
                ],
                "only the convention's jobs, keyed by bundle, in job-name order"
            );
            Ok::<_, anyhow::Error>(())
        }
        .await;
        admin
            .batch_execute(&format!("DROP DATABASE IF EXISTS {SCHEDULE_SCRATCH_DB}"))
            .await
            .expect("drop the scratch database");
        result.expect("schedule-read fixtures");
    }

    #[test]
    fn catalog_graph_sql_filters_by_membership_with_empty_meaning_all() {
        // Arrange & Act
        let sql = catalog_graph_sql(EdgeSource::Links);

        // Assert: the ends CTE (twice) and the visible CTE all use the same
        // membership test, and no nullable-scalar form survives.
        assert_eq!(sql.matches("cardinality($1::bigint[]) = 0").count(), 3);
        assert!(sql.contains("l.bundle_id = ANY($1)"));
        assert!(sql.contains("c.bundle_id = ANY($1)"));
        assert!(!sql.contains("IS NULL OR"));
    }

    #[test]
    fn catalog_graph_sql_filters_types_with_the_same_empty_means_all_shape() {
        // Arrange & Act
        let sql = catalog_graph_sql(EdgeSource::Links);

        // Assert: the type constraint is a `text[]` membership test in the
        // visible CTE, with the same cardinality guard as the bundle one.
        assert_eq!(sql.matches("cardinality($3::text[]) = 0").count(), 1);
        assert!(sql.contains("AND (cardinality($3::text[]) = 0 OR c.type = ANY($3))"));
    }

    #[test]
    fn catalog_graph_sql_counts_degree_over_exactly_the_drawn_edge_sources() {
        // Arrange & Act
        let links = catalog_graph_sql(EdgeSource::Links);
        let relationships = catalog_graph_sql(EdgeSource::Relationships);
        let both = catalog_graph_sql(EdgeSource::Both);

        // Assert: the degree's ends CTE counts link ends, relationship
        // ends, or the sum of the two - the `links` variant stays the
        // pre-relationships shape (no relationship table in sight), the
        // `rels` variant never touches pgokf.links, and `both` unions the
        // two (four end selects against the links variant's two).
        assert!(!links.contains("pgokf.current_relationships"));
        assert!(links.contains("FROM pgokf.links l"));
        assert!(!relationships.contains("pgokf.links"));
        assert_eq!(
            relationships
                .matches("FROM pgokf.current_relationships r")
                .count(),
            2
        );
        assert!(both.contains("FROM pgokf.links l"));
        assert_eq!(
            both.matches("FROM pgokf.current_relationships r").count(),
            2
        );
        assert_eq!(both.matches("UNION ALL").count(), 3);
        // Every ends branch carries the same empty-means-all bundle guard.
        assert_eq!(
            relationships
                .matches("cardinality($1::bigint[]) = 0")
                .count(),
            3
        );
        assert_eq!(both.matches("cardinality($1::bigint[]) = 0").count(), 5);
        // The executed shape: a UNION's output columns take the first
        // SELECT's names, so the relationships-first variant must alias its
        // ends to the `bundle_id, id` the deg CTE selects and groups by.
        // (The scratch-database test below proves all three variants
        // actually prepare and run; these assertions pin why they do.)
        for variant in [&relationships, &both] {
            assert!(
                variant.contains(
                    "SELECT r.source_bundle_id AS bundle_id, r.source_concept_id AS id FROM"
                ),
                "the relationship ends expose the deg CTE's column names"
            );
        }
        for variant in [&links, &relationships, &both] {
            assert!(
                variant.contains("SELECT bundle_id, id, count(*)::bigint AS degree FROM ends"),
                "the deg CTE reads ends by the names its first SELECT gives"
            );
        }
    }

    #[test]
    fn relationships_among_sql_reads_the_reader_projection_with_both_ends_drawn() {
        // Arrange & Act
        let sql = RELATIONSHIPS_AMONG_SQL;

        // Assert: the source is the reader-granted view (active
        // publications, tenant scope, and target visibility apply inline),
        // never the raw tables no API role holds a grant on; both endpoints
        // join the drawn node set; unresolved rows and self-pairs stay out;
        // the two-level fold keeps each distinct relation type with its own
        // direction (jsonb `[name, undirected]` pairs) and marks an edge
        // undirected only when every folded row is.
        assert!(sql.contains("FROM pgokf.current_relationships r"));
        assert!(!sql.contains("pgokf.relationship "));
        assert!(!sql.contains("relationship_publication"));
        assert!(sql.contains(
            "JOIN n s ON s.bundle_id = r.source_bundle_id AND s.id = r.source_concept_id"
        ));
        assert!(sql.contains(
            "JOIN n t ON t.bundle_id = r.target_bundle_id AND t.id = r.target_concept_id"
        ));
        assert!(sql.contains("NOT r.unresolved"));
        assert!(sql.contains("NOT (r.source_bundle_id = r.target_bundle_id"));
        assert!(sql.contains("r.direction = 'undirected' AS undirected"));
        assert!(sql.contains("GROUP BY 1, 2, 3, 4, 5, 6"));
        assert!(sql.contains(
            "jsonb_agg(jsonb_build_array(relation_type, undirected) ORDER BY relation_type)"
        ));
        assert!(sql.contains("bool_and(undirected)"));
        assert!(sql.contains("GROUP BY 1, 2, 3, 4 ORDER BY 1, 2, 3, 4"));
    }

    #[test]
    fn search_many_types_sql_unions_one_bounded_call_per_type() {
        // Arrange & Act
        let sql = search_many_types_sql(3);

        // Assert: one statement, one call per type with its own type slot,
        // the shared parameters referenced from every branch.
        assert_eq!(sql.matches("pgokf.concept_search(").count(), 3);
        assert_eq!(sql.matches("UNION ALL").count(), 2);
        assert!(sql.contains("concept_search($1, $2, $3, $4, $7, $8, $9, $10)"));
        assert!(sql.contains("concept_search($1, $2, $3, $5, $7, $8, $9, $10)"));
        assert!(sql.contains("concept_search($1, $2, $3, $6, $7, $8, $9, $10)"));
    }

    #[test]
    fn search_many_types_sql_leaves_the_merge_order_and_truncation_to_the_database() {
        // Arrange & Act
        let sql = search_many_types_sql(2);

        // Assert: the merged order is the function's total order computed by
        // the database - under the same collation as the per-type streams
        // and their cursor predicates, so equal-rank hits cannot page-skip
        // the way a byte-ordered Rust merge did under a non-C collation
        // (SQL `a, B` vs Rust `B, a`): the ORDER BY comes after the last
        // UNION ALL branch and the final LIMIT reuses the per-call bound.
        let order = sql.find("ORDER BY rank DESC, bundle_id, concept_id");
        let last_branch = sql.rfind("concept_search($1, $2, $3, $5");
        assert!(order.is_some_and(|at| last_branch.is_some_and(|b| at > b)));
        assert_eq!(sql.matches("ORDER BY").count(), 1);
        assert!(sql.trim_end().ends_with("LIMIT $3"));
    }

    #[test]
    fn merge_facets_sums_buckets_and_orders_like_search_facets() {
        // Arrange
        let facet = |value: &str, count: i64| Facet {
            value: value.to_owned(),
            count,
        };
        let guides = vec![facet("stable", 4), facet("draft", 1)];
        let runbooks = vec![facet("stable", 3), facet("review", 2)];

        // Act
        let merged = merge_facets(vec![guides, runbooks]);

        // Assert
        assert_eq!(
            merged,
            vec![facet("stable", 7), facet("review", 2), facet("draft", 1)]
        );
    }

    #[test]
    fn type_filter_impossible_only_when_a_group_expanded_to_nothing() {
        // Arrange
        let unfiltered = SearchQuery::default();
        let exact = SearchQuery {
            concept_types: vec!["Guide".to_owned()],
            ..SearchQuery::default()
        };
        let grouped = SearchQuery {
            type_group: Some("code".to_owned()),
            concept_types: vec!["Code Entity".to_owned()],
            ..SearchQuery::default()
        };
        let empty_group = SearchQuery {
            type_group: Some("other".to_owned()),
            ..SearchQuery::default()
        };

        // Act & Assert
        assert!(!unfiltered.type_filter_impossible());
        assert!(!exact.type_filter_impossible());
        assert!(!grouped.type_filter_impossible());
        assert!(empty_group.type_filter_impossible());
        assert!(empty_group.has_filters());
        assert!(!unfiltered.has_filters());
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

    #[tokio::test]
    async fn search_with_embedding_constrains_nothing_and_errors_without_a_server() {
        // Arrange: an unfiltered semantic query still reaches the database.
        let q = SearchQuery {
            query: "failover".to_owned(),
            ..SearchQuery::default()
        };

        // Act & Assert: the dead pool errors, proving a statement was issued.
        assert!(
            dead_db()
                .search_with_embedding(&q, &[0.1, 0.2], false)
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn an_impossible_type_group_matches_nothing_in_every_search_mode() {
        // Arrange: a group that expanded to no observed member, run through
        // every mode's entry point against a dead pool - any issued
        // statement would error instead of returning rows.
        let q = SearchQuery {
            query: "failover".to_owned(),
            type_group: Some("code".to_owned()),
            ..SearchQuery::default()
        };

        // Act
        let lexical = dead_db().search(&q, 21).await;
        let semantic = dead_db()
            .search_with_embedding(&q, &[0.1, 0.2], false)
            .await;
        let hybrid = dead_db().search_with_embedding(&q, &[0.1, 0.2], true).await;
        let browse = dead_db().browse(&q, None, 21).await;
        let facets = dead_db().facets(&q, "type").await;

        // Assert: zero results in every mode, before any statement runs.
        for rows in [lexical, semantic, hybrid] {
            assert!(rows.expect("no statement runs").is_empty());
        }
        assert!(browse.expect("no statement runs").is_empty());
        assert!(facets.expect("no statement runs").is_empty());
    }

    #[test]
    fn catalog_types_sql_is_a_complete_inventory_without_the_display_cap() {
        // Assert: membership never expands against the top-100 display
        // facets, so a type beyond the cap still joins its group.
        assert!(CATALOG_TYPES_SQL.contains("SELECT DISTINCT c.type"));
        assert!(!CATALOG_TYPES_SQL.to_uppercase().contains("LIMIT"));
    }

    #[test]
    fn embedding_search_sql_filters_by_type_inside_the_ranked_query() {
        // Assert: both modes pass the expanded exact types as the extension
        // function's trailing concept_types argument, so the filter applies
        // inside the ranked query before candidate truncation - no
        // post-filter wrapper around a truncated window.
        for sql in [embedding_search_sql(false), embedding_search_sql(true)] {
            assert!(!sql.to_uppercase().contains("WHERE"));
            assert!(!sql.contains("cardinality("));
        }
        assert!(embedding_search_sql(true).contains("concept_search_hybrid($1, $2, $3, $4, $5)"));
        assert!(embedding_search_sql(false).contains("concept_search_semantic($1, $2, $3, $4)"));
    }

    /// The database the SQL execution regression creates and drops.
    const SCRATCH_DB: &str = "pgokf_web_graph_sql_test";

    /// A connection to the local scratch server, or `None` (with a notice)
    /// when none answers - string-matched SQL tests still run, but the
    /// execution regression needs a real database. `PGOKF_WEB_TEST_DB`
    /// overrides the default `host=localhost dbname=postgres` string. The
    /// parsed config comes back so the fixture connection can reuse
    /// everything but the database name.
    async fn scratch_admin() -> Option<(tokio_postgres::Client, tokio_postgres::Config)> {
        let config: tokio_postgres::Config = std::env::var("PGOKF_WEB_TEST_DB")
            .unwrap_or_else(|_| {
                let user = std::env::var("USER").unwrap_or_else(|_| "postgres".to_owned());
                format!("host=localhost dbname=postgres user={user}")
            })
            .parse()
            .expect("PGOKF_WEB_TEST_DB parses as a libpq connection string");
        let (client, connection) = match config.connect(NoTls).await {
            Ok(pair) => pair,
            Err(error) => {
                eprintln!(
                    "skipping the graph SQL execution test: no scratch PostgreSQL answers ({error})"
                );
                return None;
            }
        };
        tokio::spawn(async move {
            if let Err(error) = connection.await {
                eprintln!("scratch admin connection error: {error}");
            }
        });
        Some((client, config))
    }

    /// Every graph SQL variant prepared and executed against a real
    /// database over minimal fixtures, not string-matched: the
    /// relationships-only ends CTE once failed at prepare time (its output
    /// columns took the relationship table's names, not the `bundle_id, id`
    /// the degree CTE groups by) while every string assertion passed.
    #[tokio::test]
    async fn graph_sql_executes_against_a_scratch_database() {
        let Some((admin, config)) = scratch_admin().await else {
            return;
        };
        admin
            .batch_execute(&format!("DROP DATABASE IF EXISTS {SCRATCH_DB}"))
            .await
            .expect("drop a stale scratch database");
        admin
            .batch_execute(&format!("CREATE DATABASE {SCRATCH_DB}"))
            .await
            .expect("create the scratch database");
        let result = run_graph_sql_fixtures(config).await;
        admin
            .batch_execute(&format!("DROP DATABASE IF EXISTS {SCRATCH_DB}"))
            .await
            .expect("drop the scratch database");
        result.expect("graph SQL fixtures");
    }

    /// The scratch schema and rows the execution regression runs over:
    /// every object the graph SQL references, with a mixed directed +
    /// undirected pair, a wholly undirected pair, and self/unresolved rows
    /// that must never fold in.
    const SCRATCH_FIXTURES: &str = "
        CREATE SCHEMA pgokf;
        CREATE TABLE pgokf.bundles (id bigint PRIMARY KEY, name text, path text, enabled boolean, retired_at timestamptz);
        CREATE TABLE pgokf.concepts (bundle_id bigint, id text, title text, type text, path text);
        CREATE TABLE pgokf.links (bundle_id bigint, source_id text, target_id text, resolved boolean);
        CREATE TABLE pgokf.current_relationships (
            source_bundle_id bigint, source_concept_id text, relation_type text, direction text,
            target_bundle_id bigint, target_concept_id text, unresolved boolean
        );
        INSERT INTO pgokf.bundles VALUES (1, 'one', '/one', true, null), (2, 'two', '/two', true, null);
        INSERT INTO pgokf.concepts VALUES
            (1, 'a', 'A', 'Guide', 'a'), (1, 'b', 'B', 'Guide', 'b'),
            (1, 'c', 'C', 'Guide', 'c'), (1, 'd', 'D', 'Guide', 'd'),
            (2, 'b', 'B two', 'Guide', 'b');
        INSERT INTO pgokf.links VALUES (1, 'a', 'b', true), (1, 'a', 'c', true);
        INSERT INTO pgokf.current_relationships VALUES
            (1, 'a', 'probe:directed', 'directed', 2, 'b', false),
            (1, 'a', 'probe:undirected', 'undirected', 2, 'b', false),
            (1, 'b', 'probe:only-undirected', 'undirected', 1, 'c', false),
            (1, 'a', 'probe:self', 'directed', 1, 'a', false),
            (1, 'a', 'probe:unresolved', 'directed', null, null, true);";

    /// The fixture body of [`graph_sql_executes_against_a_scratch_database`],
    /// run inside one rolled-back transaction in the scratch database.
    async fn run_graph_sql_fixtures(mut config: tokio_postgres::Config) -> Result<()> {
        config.dbname(SCRATCH_DB);
        let (mut client, connection) = config
            .connect(NoTls)
            .await
            .context("connect to the scratch database")?;
        let connection = tokio::spawn(async move {
            if let Err(error) = connection.await {
                eprintln!("scratch connection error: {error}");
            }
        });
        let outcome = async {
            let tx = client.transaction().await?;
            tx.batch_execute(SCRATCH_FIXTURES).await?;
            check_catalog_degrees(&tx).await?;
            check_relationship_fold(&tx).await?;
            tx.rollback().await?;
            Ok(())
        }
        .await;
        drop(client);
        let _ = connection.await;
        outcome
    }

    /// All three degree variants prepare and execute; degrees count exactly
    /// the selected sources' ends (self-pairs and unresolved rows never
    /// count). Nodes sort by (`bundle_id`, `id`) here.
    async fn check_catalog_degrees(tx: &tokio_postgres::Transaction<'_>) -> Result<()> {
        let no_bundles: Vec<i64> = Vec::new();
        let no_types: Vec<String> = Vec::new();
        for (variant, expected) in [
            (
                EdgeSource::Links,
                vec![
                    (1, "a", 2),
                    (1, "b", 1),
                    (1, "c", 1),
                    (1, "d", 0),
                    (2, "b", 0),
                ],
            ),
            (
                EdgeSource::Relationships,
                vec![
                    (1, "a", 2),
                    (1, "b", 1),
                    (1, "c", 1),
                    (1, "d", 0),
                    (2, "b", 2),
                ],
            ),
            (
                EdgeSource::Both,
                vec![
                    (1, "a", 4),
                    (1, "b", 2),
                    (1, "c", 2),
                    (1, "d", 0),
                    (2, "b", 2),
                ],
            ),
        ] {
            let rows = tx
                .query(
                    &catalog_graph_sql(variant),
                    &[&no_bundles, &300_i64, &no_types],
                )
                .await
                .with_context(|| format!("the {variant:?} catalog graph SQL executes"))?;
            let mut degrees: Vec<(i64, String, i64)> = rows
                .iter()
                .map(|r| Ok((col(r, 0)?, col(r, 2)?, col(r, 7)?)))
                .collect::<Result<_>>()?;
            degrees.sort();
            let mut expected: Vec<(i64, String, i64)> = expected
                .into_iter()
                .map(|(b, id, d)| (b, id.to_owned(), d))
                .collect();
            expected.sort();
            assert_eq!(
                degrees, expected,
                "the {variant:?} variant's per-node degrees"
            );
            assert_eq!(col::<i64>(&rows[0], 8)?, 5, "every concept is visible");
        }
        Ok(())
    }

    /// The fold keeps each relation type's own direction: the mixed pair
    /// (1,a)-(2,b) lists its undirected member under the fold's arrow, and
    /// the wholly undirected pair (1,b)-(1,c) folds to undirected.
    /// Self-pairs and unresolved rows stay out.
    async fn check_relationship_fold(tx: &tokio_postgres::Transaction<'_>) -> Result<()> {
        let node_bundles = vec![1_i64, 1, 1, 1, 2];
        let node_ids = vec!["a", "b", "c", "d", "b"];
        let rows = tx
            .query(RELATIONSHIPS_AMONG_SQL, &[&node_bundles, &node_ids])
            .await
            .context("the relationships-among SQL executes")?;
        assert_eq!(rows.len(), 2, "two drawn pairs, self/unresolved out");

        let mixed = &rows[0];
        let mixed_types: Vec<(String, bool)> =
            serde_json::from_value(col(mixed, 5)?).context("the fold's jsonb pairs parse")?;
        assert_eq!(
            (
                col::<i64>(mixed, 0)?,
                col::<String>(mixed, 1)?,
                col::<i64>(mixed, 2)?,
                col::<String>(mixed, 3)?,
                col::<i64>(mixed, 4)?,
            ),
            (1, "a".to_owned(), 2, "b".to_owned(), 2),
            "the mixed pair folds both rows"
        );
        assert_eq!(
            mixed_types,
            vec![
                ("probe:directed".to_owned(), false),
                ("probe:undirected".to_owned(), true),
            ],
            "each folded type keeps its own direction"
        );
        assert!(!col::<bool>(mixed, 6)?, "a mixed fold keeps its arrow");

        let undirected = &rows[1];
        let undirected_types: Vec<(String, bool)> = serde_json::from_value(col(undirected, 5)?)?;
        assert_eq!(
            (
                col::<i64>(undirected, 0)?,
                col::<String>(undirected, 1)?,
                col::<i64>(undirected, 2)?,
                col::<String>(undirected, 3)?,
                col::<i64>(undirected, 4)?,
            ),
            (1, "b".to_owned(), 1, "c".to_owned(), 1),
            "the wholly undirected pair folds alone"
        );
        assert_eq!(
            undirected_types,
            vec![("probe:only-undirected".to_owned(), true)],
        );
        assert!(
            col::<bool>(undirected, 6)?,
            "every folded row is undirected"
        );
        Ok(())
    }
}
