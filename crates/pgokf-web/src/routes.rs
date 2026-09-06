// SPDX-License-Identifier: AGPL-3.0-only
//! HTTP handlers: HTML pages rendered from Askama templates, the htmx
//! partials that update them in place, and a small JSON API over the same
//! data layer.
//!
//! Handlers hold no catalogue semantics: every list, filter, rank, and
//! visibility decision is the database's. A page is a projection of what the
//! pooled reader role is allowed to see.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, LazyLock};
use std::time::Duration;

use askama::Template;
use axum::body::Body;
use axum::extract::{Path, Query, Request, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use pgokf_companion::embeddings::EmbeddingsClient;
use serde::Deserialize;
use serde_json::Value;
use tower::limit::ConcurrencyLimitLayer;

use crate::db::{
    BundleInfo, BundleLogEntry, BundleStat, ConceptDetail, ConceptSummary, Cursor, Db,
    DuplicateGroup, Facet, Failure, Graph, Hit, Link, Neighbor, SearchQuery, StaleConcept,
    SyncLogEntry, Version,
};
use crate::graph::{GraphEdge, GraphNode};
use crate::links::Resolver;
use crate::{graph, markdown};

/// Shared application state.
pub(crate) struct App {
    pub db: Db,
    pub embedder: Option<EmbeddingsClient>,
    pub catalog_name: String,
    pub tenant: Option<String>,
    /// The library version seen at startup, for the footer.
    pub version: String,
}

type Shared = Arc<App>;

/// Wall-clock bound on one request, covering every statement it issues.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// Requests handled at once; the rest queue (and time out) rather than
/// piling onto the connection pool.
const MAX_IN_FLIGHT: usize = 64;

/// Build the router.
pub(crate) fn router(app: Shared) -> Router {
    let api = Router::new()
        .route("/health", get(api_health))
        .route("/bundles", get(api_bundles))
        .route("/search", get(api_search))
        .route("/concepts/{bundle_id}/{*concept_id}", get(api_concept))
        .route("/graph/{bundle_id}/{*concept_id}", get(api_graph))
        .fallback(not_found);
    // Layers wrap inside-out: the concurrency limit sits inside the request
    // timeout so time spent waiting for a slot counts against the bound,
    // and error shaping is outermost so even a timeout is rendered for the
    // caller (JSON under /api, the error page elsewhere).
    Router::new()
        .route("/", get(dashboard))
        .route("/search", get(search_page))
        .route("/search/results", get(search_results))
        .route("/bundles", get(bundles_page))
        .route("/bundles/{id}", get(bundle_page))
        .route("/status", get(status_page))
        .route("/concepts/{bundle_id}/{*concept_id}", get(concept_page))
        .route("/source/{bundle_id}/{*concept_id}", get(concept_source))
        .route("/static/{file}", get(static_asset))
        .nest("/api", api)
        .fallback(not_found)
        .layer(ConcurrencyLimitLayer::new(MAX_IN_FLIGHT))
        .layer(middleware::from_fn(request_timeout))
        .layer(middleware::from_fn(security_headers))
        .layer(middleware::from_fn(shape_errors))
        .with_state(app)
}

// ---------------------------------------------------------------------------
// Middleware
// ---------------------------------------------------------------------------

/// The Content-Security-Policy every page carries. Only same-origin
/// scripts, styles, and images (plus inline `data:` images for the favicon)
/// are admitted, so an escape from the sanitizer would still have nowhere
/// to run or report to. The first three style hashes are the stylesheets
/// the vendored `3d-force-graph.min.js` 1.80.0 injects at load (cursor and
/// nav-info rules); the last, with `'unsafe-hashes'`, is the hash of the
/// empty string, for the empty `style` text its unused tooltip assigns.
/// They must be refreshed when that file is upgraded.
const CONTENT_SECURITY_POLICY: &str = "default-src 'self'; img-src 'self' data:; \
    style-src 'self' 'sha256-9xjtvxMT1ApHlgn9ohbh2FNfvK5Tqtzy94BjfXBeMSY=' \
    'sha256-0/4q5IwejFb2zgHlQwwtwmGHS8ZbXE1kmz/TkRFlZ7M=' \
    'sha256-yfc2FhpkFR0EAy3T+zDsaAFGXSP9B3ELNvaJKDzNhkk=' \
    'unsafe-hashes' 'sha256-47DEQpj8HBSa+/TImW+5JCeuQeRkm5NMpJWZG3hSuFU='; \
    script-src 'self'; object-src 'none'; base-uri 'self'; form-action 'self'; \
    frame-ancestors 'none'";

/// Response headers every page carries.
async fn security_headers(request: Request, next: Next) -> Response {
    let mut response = next.run(request).await;
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(CONTENT_SECURITY_POLICY),
    );
    headers.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("same-origin"),
    );
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(header::X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
    response
}

/// Bound a whole request; a page that issues a dozen statements otherwise
/// answers only to the per-statement timeout.
async fn request_timeout(request: Request, next: Next) -> Response {
    match tokio::time::timeout(REQUEST_TIMEOUT, next.run(request)).await {
        Ok(response) => response,
        Err(_) => AppError::timeout().into_response(),
    }
}

/// Give every error response its final shape. Under `/api/` that is a JSON
/// document; elsewhere it is the error page. Handler errors carry their
/// message as a response extension; axum's own rejections (a non-numeric
/// bundle id, a duplicated query field) and empty-bodied statuses (405)
/// arrive as plain text and are re-rendered the same way.
async fn shape_errors(request: Request, next: Next) -> Response {
    let path = request.uri().path();
    let is_api = path == "/api" || path.starts_with("/api/");
    let response = next.run(request).await;
    let status = response.status();
    if !(status.is_client_error() || status.is_server_error()) {
        return response;
    }
    let content_type = response
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_owned();
    let already_shaped = if is_api {
        content_type.starts_with("application/json")
    } else {
        content_type.starts_with("text/html")
    };
    if already_shaped {
        return response;
    }
    let message = match response.extensions().get::<ErrorDetail>() {
        Some(detail) => detail.0.clone(),
        None => match axum::body::to_bytes(response.into_body(), 4096).await {
            Ok(bytes) => String::from_utf8_lossy(&bytes).trim().to_owned(),
            Err(_) => String::new(),
        },
    };
    let message = if message.is_empty() {
        status.canonical_reason().unwrap_or("error").to_owned()
    } else {
        message
    };
    if is_api {
        let body = serde_json::json!({
            "error": { "status": status.as_u16(), "message": message }
        });
        (status, Json(body)).into_response()
    } else {
        AppError { status, message }.into_response()
    }
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// The user-facing message of a failed request, attached to the response so
/// the API layer can re-encode it.
#[derive(Debug, Clone)]
struct ErrorDetail(String);

/// A request failure rendered as an error page (or JSON under `/api/`).
pub(crate) struct AppError {
    status: StatusCode,
    message: String,
}

impl AppError {
    fn not_found(what: &str) -> Self {
        Self {
            status: StatusCode::NOT_FOUND,
            message: format!("{what} is not visible to this session or does not exist."),
        }
    }

    fn bad_request(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            message: message.into(),
        }
    }

    fn timeout() -> Self {
        Self {
            status: StatusCode::GATEWAY_TIMEOUT,
            message: "The catalog took too long to answer; try a narrower query.".to_owned(),
        }
    }

    fn bad_gateway(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::BAD_GATEWAY,
            message: message.into(),
        }
    }
}

impl From<anyhow::Error> for AppError {
    fn from(error: anyhow::Error) -> Self {
        // The operator sees the cause in the log; the page sees a summary.
        eprintln!("pgokf-web: request failed: {error:#}");
        match crate::db::classify(&error) {
            Failure::Busy => Self {
                status: StatusCode::SERVICE_UNAVAILABLE,
                message: "The catalog is busy; try again in a moment.".to_owned(),
            },
            Failure::Timeout => Self::timeout(),
            Failure::InvalidInput => Self::bad_request(
                crate::db::db_message(&error)
                    .unwrap_or_else(|| "The catalog rejected a request value.".to_owned()),
            ),
            Failure::Other => Self {
                status: StatusCode::INTERNAL_SERVER_ERROR,
                message: "The catalog query failed; the server log has the cause.".to_owned(),
            },
        }
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let page = ErrorPage {
            shell: Shell::bare("Error"),
            status: self.status.as_u16(),
            message: self.message.clone(),
        };
        let mut response = match page.render() {
            Ok(html) => (self.status, Html(html)).into_response(),
            Err(_) => (self.status, self.message.clone()).into_response(),
        };
        response.extensions_mut().insert(ErrorDetail(self.message));
        response
    }
}

type PageResult = Result<Response, AppError>;

fn html<T: Template>(template: &T) -> PageResult {
    let body = template
        .render()
        .map_err(|error| anyhow::anyhow!("template rendering failed: {error}"))?;
    Ok(Html(body).into_response())
}

// ---------------------------------------------------------------------------
// View models
// ---------------------------------------------------------------------------

/// Cache-busting token for the embedded static assets: a hash of their
/// contents, so a deploy never pairs new templates with a cached old script.
static ASSET_VERSION: LazyLock<String> = LazyLock::new(|| {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for chunk in [APP_CSS, APP_JS, BOOT_JS, GRAPH_JS, HTMX_JS, FORCE_GRAPH_JS] {
        for byte in chunk.bytes() {
            hash ^= u64::from(byte);
            hash = hash.wrapping_mul(0x0100_0000_01b3);
        }
    }
    format!("{hash:016x}")[..12].to_owned()
});

/// The header and navigation state every page carries.
pub(crate) struct Shell {
    pub page_title: String,
    pub catalog_name: String,
    pub nav: String,
    pub query: String,
    /// Active search filters carried by the top search bar as hidden inputs,
    /// so a new query typed there keeps them.
    pub filters: Vec<(String, String)>,
    pub tenant: Option<String>,
    pub version: String,
    pub asset_version: String,
}

impl Shell {
    fn new(app: &App, page_title: &str, nav: &'static str) -> Self {
        Self {
            page_title: page_title.to_owned(),
            catalog_name: app.catalog_name.clone(),
            nav: nav.to_owned(),
            query: String::new(),
            filters: Vec::new(),
            tenant: app.tenant.clone(),
            version: app.version.clone(),
            asset_version: ASSET_VERSION.clone(),
        }
    }

    fn bare(page_title: &str) -> Self {
        Self {
            page_title: page_title.to_owned(),
            catalog_name: "pgokf".to_owned(),
            nav: String::new(),
            query: String::new(),
            filters: Vec::new(),
            tenant: None,
            version: env!("CARGO_PKG_VERSION").to_owned(),
            asset_version: ASSET_VERSION.clone(),
        }
    }
}

/// `pgokf.health()` flattened for templates.
// These mirror the boolean fields of the JSON document one to one; encoding
// them as enums would only move the flattening into the templates.
#[allow(clippy::struct_excessive_bools)]
pub(crate) struct HealthView {
    pub ok: bool,
    pub roles_ok: bool,
    pub config_ok: bool,
    pub bundle_count: i64,
    pub concept_count: i64,
    pub search_backend: String,
    pub bm25_ready: bool,
    pub tenant_required: bool,
    pub in_recovery: bool,
    pub version: String,
    pub sql_version: String,
}

impl HealthView {
    fn from_json(value: &Value, version: String, sql_version: String) -> Self {
        Self {
            ok: value["ok"].as_bool().unwrap_or(false),
            roles_ok: value["roles_ok"].as_bool().unwrap_or(false),
            config_ok: value["config_ok"].as_bool().unwrap_or(false),
            bundle_count: value["bundle_count"].as_i64().unwrap_or(0),
            concept_count: value["concept_count"].as_i64().unwrap_or(0),
            search_backend: value["search_backend"]
                .as_str()
                .unwrap_or("native")
                .to_owned(),
            bm25_ready: value["bm25_ready"].as_bool().unwrap_or(false),
            tenant_required: value["tenant_required"].as_bool().unwrap_or(false),
            in_recovery: value["in_recovery"].as_bool().unwrap_or(false),
            version,
            sql_version,
        }
    }
}

/// `pgokf.search_index_status()` flattened for templates.
#[allow(clippy::struct_excessive_bools)]
pub(crate) struct IndexView {
    pub bm25_available: bool,
    pub bm25_provider: Option<String>,
    pub bm25_provider_setting: String,
    pub bm25_index_exists: bool,
    pub bm25_indexed_rows: i64,
    pub bm25_total_rows: i64,
    pub embedding_available: bool,
    pub embedding_index_exists: bool,
    pub embedded_rows: i64,
    pub total_concepts: i64,
    pub embedding_coverage: String,
    /// Coverage as a whole-number percentage for the progress meter.
    pub embedding_coverage_pct: i64,
    pub embedding_dim: i64,
}

impl IndexView {
    fn from_json(value: &Value) -> Self {
        let bm25 = &value["bm25"];
        let embedding = &value["embedding"];
        let pct = embedding["coverage_pct"].as_f64().unwrap_or(0.0);
        Self {
            bm25_available: bm25["available"].as_bool().unwrap_or(false),
            bm25_provider: bm25["provider"].as_str().map(str::to_owned),
            bm25_provider_setting: bm25["provider_setting"]
                .as_str()
                .unwrap_or("auto")
                .to_owned(),
            bm25_index_exists: bm25["index_exists"].as_bool().unwrap_or(false),
            bm25_indexed_rows: bm25["indexed_rows"].as_i64().unwrap_or(0),
            bm25_total_rows: bm25["total_rows"].as_i64().unwrap_or(0),
            embedding_available: embedding["pgvector_available"].as_bool().unwrap_or(false),
            embedding_index_exists: embedding["index_exists"].as_bool().unwrap_or(false),
            embedded_rows: embedding["embedded_rows"].as_i64().unwrap_or(0),
            total_concepts: embedding["total_concepts"].as_i64().unwrap_or(0),
            embedding_coverage: format!("{pct:.0}%"),
            // Bounded to 0..=100 by the extension; the cast only formats it.
            #[allow(clippy::cast_possible_truncation)]
            embedding_coverage_pct: pct.round().clamp(0.0, 100.0) as i64,
            embedding_dim: embedding["dim"].as_i64().unwrap_or(0),
        }
    }
}

/// A result row as the hit list shows it: a ranked search hit or a browsed
/// concept (no rank, description as the snippet).
pub(crate) struct HitView {
    pub bundle_id: i64,
    pub bundle_name: String,
    pub concept_id: String,
    pub path: String,
    pub title: Option<String>,
    pub concept_type: Option<String>,
    pub rank: Option<f32>,
    pub rank_display: Option<String>,
    pub headline_html: Option<String>,
    pub tags: Vec<String>,
    pub href: String,
}

impl HitView {
    fn from_hit(hit: Hit, bundle_names: &BTreeMap<i64, String>) -> Self {
        Self {
            bundle_name: bundle_name(bundle_names, hit.bundle_id),
            href: concept_href(hit.bundle_id, &hit.concept_id),
            bundle_id: hit.bundle_id,
            concept_id: hit.concept_id,
            path: hit.path,
            title: hit.title,
            concept_type: hit.concept_type,
            rank: Some(hit.rank),
            rank_display: Some(format!("{:.3}", hit.rank)),
            headline_html: hit.headline.as_deref().map(markdown::headline),
            tags: Vec::new(),
        }
    }

    fn from_summary(c: ConceptSummary, bundle_names: &BTreeMap<i64, String>) -> Self {
        Self {
            bundle_name: bundle_name(bundle_names, c.bundle_id),
            href: concept_href(c.bundle_id, &c.concept_id),
            bundle_id: c.bundle_id,
            concept_id: c.concept_id,
            path: c.path,
            title: c.title,
            concept_type: c.concept_type,
            rank: None,
            rank_display: None,
            headline_html: c.description.as_deref().map(markdown::escape),
            tags: c.tags,
        }
    }
}

fn bundle_name(bundle_names: &BTreeMap<i64, String>, bundle_id: i64) -> String {
    bundle_names
        .get(&bundle_id)
        .cloned()
        .unwrap_or_else(|| format!("bundle {bundle_id}"))
}

/// The result list plus its heading and pagination state.
pub(crate) struct ResultsView {
    pub hits: Vec<HitView>,
    pub heading: String,
    pub summary: String,
    pub notice: Option<String>,
    pub next_url: Option<String>,
    pub next_partial_url: Option<String>,
    /// `true` when the list is a filter-only browse rather than a ranked search.
    pub browsing: bool,
    /// `true` when the requested mode could not be served and lexical
    /// results were shown instead.
    pub degraded: bool,
}

impl ResultsView {
    fn empty(heading: &str, summary: &str) -> Self {
        Self {
            hits: Vec::new(),
            heading: heading.to_owned(),
            summary: summary.to_owned(),
            notice: None,
            next_url: None,
            next_partial_url: None,
            browsing: false,
            degraded: false,
        }
    }
}

/// One facet bucket with the URL that applies it and whether it is active.
pub(crate) struct FacetLink {
    pub label: String,
    pub count: i64,
    pub href: String,
    pub active: bool,
}

/// One `<option>` of a facet select: the bucket plus whether it is chosen.
pub(crate) struct FacetOption {
    pub value: String,
    pub label: String,
    pub selected: bool,
}

/// Facet buckets for the sidebar.
pub(crate) struct FacetsView {
    pub types: Vec<Facet>,
    pub tags: Vec<FacetLink>,
    pub bundles: Vec<FacetLink>,
    pub statuses: Vec<FacetOption>,
    pub trust_tiers: Vec<FacetOption>,
}

impl FacetsView {
    fn empty() -> Self {
        Self {
            types: Vec::new(),
            tags: Vec::new(),
            bundles: Vec::new(),
            statuses: Vec::new(),
            trust_tiers: Vec::new(),
        }
    }
}

/// Select options for a facet, keeping the current choice selectable even
/// when no result carries it (so the filter can still be cleared).
fn facet_options(buckets: Vec<Facet>, current: &str) -> Vec<FacetOption> {
    let mut options: Vec<FacetOption> = buckets
        .into_iter()
        .map(|f| FacetOption {
            selected: f.value == current,
            label: format!("{} ({})", f.value, f.count),
            value: f.value,
        })
        .collect();
    if !current.is_empty() && !options.iter().any(|o| o.selected) {
        options.push(FacetOption {
            value: current.to_owned(),
            label: current.to_owned(),
            selected: true,
        });
    }
    options
}

/// The provenance tab's identity rows, read across the OKF provenance
/// families: `generated` (who produced the current content, and when; a
/// human as readily as an agent) is shown as "Created", and the pgokf
/// `author` / `owner` metadata keys as themselves when declared.
pub(crate) struct ProvenanceView {
    pub created_by: Option<String>,
    pub created_at: Option<String>,
    /// `generated.model`, when the producer recorded one.
    pub model: Option<String>,
    pub author: Option<String>,
    pub owner: Option<String>,
}

impl ProvenanceView {
    fn from_concept(c: &ConceptDetail) -> Self {
        let generated = c.provenance.as_ref().map(|p| &p.details["generated"]);
        Self {
            created_by: c.provenance.as_ref().and_then(|p| p.generated_by.clone()),
            created_at: c.provenance.as_ref().and_then(|p| p.generated_at.clone()),
            model: generated
                .and_then(|g| g["model"].as_str())
                .map(str::to_owned),
            author: c.metadata.get("author").and_then(actor_display),
            owner: c.metadata.get("owner").and_then(actor_display),
        }
    }
}

/// An actor field as text: the string itself, or for the mapping form the
/// spec allows ("display metadata around the same actor string") its
/// `id`, `actor`, or `name`, else the compact JSON.
fn actor_display(value: &Value) -> Option<String> {
    match value {
        Value::String(s) => Some(s.clone()),
        Value::Object(map) => ["id", "actor", "name"]
            .iter()
            .find_map(|key| map.get(*key).and_then(Value::as_str))
            .map(str::to_owned)
            .or_else(|| Some(value.to_string())),
        Value::Null => None,
        other => Some(other.to_string()),
    }
}

/// One custom-metadata row, value pretty-printed.
pub(crate) struct MetadataRow {
    pub key: String,
    pub value: String,
}

/// The link relation OKF assigns to an ordinary Markdown link; it carries no
/// information on the links tab, so only other relations are shown.
const DEFAULT_RELATION: &str = "reference";

/// Label for concepts that sit directly in the bundle root.
const TOP_LEVEL_DIRECTORY: &str = "top level";

/// Where a link edge points, as the links tab shows it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LinkState {
    /// Resolves to a concept the reader can open.
    Resolved,
    /// A scheme-qualified URL outside the catalog.
    External,
    /// An internal path that no concept matches.
    Unresolved,
}

/// One row of the links tab: every edge between this concept and one
/// counterpart, folded together so a page that cites the same document five
/// times lists it once with a count.
#[derive(Debug, Clone)]
pub(crate) struct LinkGroupView {
    /// Concept page of the counterpart, when it resolves.
    pub href: Option<String>,
    /// Counterpart title, falling back to the first link text or the path.
    pub label: String,
    /// The raw target for external and unresolved links.
    pub detail: Option<String>,
    /// Distinct non-default relations carried by the folded edges.
    pub relations: Vec<String>,
    pub count: usize,
    pub state: LinkState,
    /// The edge points back at the concept itself (an in-page anchor).
    pub is_self: bool,
}

impl LinkGroupView {
    pub fn is_resolved(&self) -> bool {
        self.state == LinkState::Resolved
    }

    pub fn is_external(&self) -> bool {
        self.state == LinkState::External
    }
}

/// Which end of the edge the current concept sits at.
#[derive(Debug, Clone, Copy)]
enum LinkDirection {
    Outgoing,
    Incoming,
}

/// Fold a concept's edges by counterpart, preserving first-seen order.
fn group_links(links: &[Link], direction: LinkDirection, self_id: &str) -> Vec<LinkGroupView> {
    let mut groups: Vec<LinkGroupView> = Vec::new();
    let mut index: BTreeMap<String, usize> = BTreeMap::new();
    for link in links {
        let counterpart = match direction {
            LinkDirection::Outgoing => link.target_id.as_deref(),
            LinkDirection::Incoming => Some(link.source_id.as_str()),
        };
        let path = link.target_path.as_deref();
        let (state, key) = match counterpart {
            Some(id) if link.resolved => (LinkState::Resolved, format!("c:{id}")),
            _ if link.is_external => (
                LinkState::External,
                format!("x:{}", link.text.as_deref().unwrap_or_default()),
            ),
            _ => (
                LinkState::Unresolved,
                format!("u:{}", path.unwrap_or_default()),
            ),
        };
        let relation = (link.relation != DEFAULT_RELATION).then(|| link.relation.clone());
        if let Some(&i) = index.get(&key) {
            groups[i].count += 1;
            if let Some(relation) = relation
                && !groups[i].relations.contains(&relation)
            {
                groups[i].relations.push(relation);
            }
            continue;
        }
        let text = link.text.as_deref().filter(|t| !t.trim().is_empty());
        let label = match state {
            LinkState::Resolved => link
                .counterpart_title
                .clone()
                .or_else(|| text.map(str::to_owned))
                .or_else(|| counterpart.map(str::to_owned))
                .unwrap_or_default(),
            LinkState::External | LinkState::Unresolved => {
                text.or(path).unwrap_or_default().to_owned()
            }
        };
        let href = match (state, counterpart) {
            (LinkState::Resolved, Some(id)) => Some(concept_href(link.bundle_id, id)),
            _ => None,
        };
        index.insert(key, groups.len());
        groups.push(LinkGroupView {
            href,
            label,
            detail: (state == LinkState::Unresolved).then(|| path.unwrap_or_default().to_owned()),
            relations: relation.into_iter().collect(),
            count: 1,
            state,
            is_self: state == LinkState::Resolved && counterpart == Some(self_id),
        });
    }
    groups
}

/// Concepts of one directory inside a bundle.
pub(crate) struct DirGroup {
    pub directory: String,
    pub concepts: Vec<ConceptSummary>,
}

// ---------------------------------------------------------------------------
// Templates
// ---------------------------------------------------------------------------

#[derive(Template)]
#[template(path = "dashboard.html")]
struct DashboardPage {
    shell: Shell,
    health: HealthView,
    index: IndexView,
    stats: Vec<BundleStat>,
    sync_log: Vec<SyncLogEntry>,
}

#[derive(Template)]
#[template(path = "search.html")]
struct SearchPage {
    shell: Shell,
    form: SearchForm,
    bundles: Vec<BundleInfo>,
    facets: FacetsView,
    results: ResultsView,
    semantic_available: bool,
    backend_label: String,
}

#[derive(Template)]
#[template(path = "partials/results.html")]
struct ResultsPartial {
    form: SearchForm,
    facets: FacetsView,
    results: ResultsView,
    /// `true` for a "load more" request: only the extra rows are wanted.
    append: bool,
    /// The partial also refreshes the sidebar facets out of band; the full
    /// page renders them in place instead.
    oob: bool,
}

#[derive(Template)]
#[template(path = "bundles.html")]
struct BundlesPage {
    shell: Shell,
    stats: Vec<BundleStat>,
}

#[derive(Template)]
#[template(path = "bundle.html")]
struct BundlePage {
    shell: Shell,
    bundle: BundleInfo,
    groups: Vec<DirGroup>,
    page_size: usize,
    shown: usize,
    /// Link to the next page of concepts, when the bundle has more.
    next_url: Option<String>,
    /// `true` when this page is not the first.
    continued: bool,
    sync_log: Vec<SyncLogEntry>,
    bundle_log: Vec<BundleLogEntry>,
}

#[derive(Template)]
#[template(path = "concept.html")]
struct ConceptPage {
    shell: Shell,
    c: ConceptDetail,
    body_html: String,
    metadata: Vec<MetadataRow>,
    outgoing: Vec<LinkGroupView>,
    incoming: Vec<LinkGroupView>,
    neighbors: Vec<Neighbor>,
    graph_svg: String,
    hops: i32,
    hops_options: Vec<(i32, bool)>,
    self_href: String,
    /// JSON endpoint of the interactive graph for this concept.
    graph_url: String,
    prov: ProvenanceView,
    similar: Vec<HitView>,
    history: Vec<Version>,
}

#[derive(Template)]
#[template(path = "status.html")]
struct StatusPage {
    shell: Shell,
    health: HealthView,
    index: IndexView,
    config_json: String,
    stale: Vec<StaleConcept>,
    duplicates: Vec<DuplicateGroup>,
    sync_log: Vec<SyncLogEntry>,
}

#[derive(Template)]
#[template(path = "error.html")]
struct ErrorPage {
    shell: Shell,
    status: u16,
    message: String,
}

// ---------------------------------------------------------------------------
// Search parameters
// ---------------------------------------------------------------------------

/// Raw query-string parameters of the search page and its partial.
#[derive(Debug, Clone, Default, Deserialize)]
pub(crate) struct SearchParams {
    #[serde(default)]
    q: String,
    #[serde(default)]
    bundle: String,
    #[serde(default, rename = "type")]
    concept_type: String,
    #[serde(default)]
    tags: String,
    #[serde(default)]
    status: String,
    #[serde(default)]
    trust: String,
    #[serde(default)]
    mode: String,
    #[serde(default)]
    limit: String,
    #[serde(default)]
    after_rank: String,
    #[serde(default)]
    after_bundle: String,
    #[serde(default)]
    after_id: String,
    /// Set by "load more": render only the additional rows.
    #[serde(default)]
    append: String,
}

/// The normalized form state echoed back into the filters.
#[derive(Debug, Clone)]
pub(crate) struct SearchForm {
    pub q: String,
    pub bundle: String,
    pub concept_type: String,
    pub tags: String,
    pub status: String,
    pub trust: String,
    pub mode: String,
    pub limit: String,
    /// `(value, selected)` for the page-size selector.
    pub limit_options: Vec<(String, bool)>,
}

impl SearchForm {
    /// The non-query fields as `(name, value)` pairs, for hidden inputs and
    /// generated URLs; defaults are left out so URLs stay short.
    fn filter_pairs(&self) -> Vec<(String, String)> {
        let default_limit = DEFAULT_LIMIT.to_string();
        [
            ("bundle", &self.bundle),
            ("type", &self.concept_type),
            ("tags", &self.tags),
            ("status", &self.status),
            ("trust", &self.trust),
            ("mode", &self.mode),
            ("limit", &self.limit),
        ]
        .into_iter()
        .filter(|(key, value)| match *key {
            _ if value.is_empty() => false,
            "mode" => *value != "lexical",
            "limit" => **value != default_limit,
            _ => true,
        })
        .map(|(key, value)| (key.to_owned(), value.clone()))
        .collect()
    }

    /// This form with one more tag required.
    fn with_tag(&self, tag: &str) -> Self {
        let mut tags: Vec<&str> = self
            .tags
            .split(',')
            .map(str::trim)
            .filter(|t| !t.is_empty())
            .collect();
        if !tags.contains(&tag) {
            tags.push(tag);
        }
        Self {
            tags: tags.join(", "),
            ..self.clone()
        }
    }

    /// This form narrowed to one bundle.
    fn with_bundle(&self, bundle_id: &str) -> Self {
        Self {
            bundle: bundle_id.to_owned(),
            ..self.clone()
        }
    }

    fn has_tag(&self, tag: &str) -> bool {
        self.tags.split(',').map(str::trim).any(|t| t == tag)
    }
}

const DEFAULT_LIMIT: i32 = 20;
const LIMIT_OPTIONS: [i32; 4] = [10, 20, 50, 100];
const MAX_LIMIT: i32 = 100;

fn non_empty(value: &str) -> Option<String> {
    let trimmed = value.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_owned())
}

impl SearchParams {
    /// Validate and normalize into the typed query the data layer runs.
    fn normalize(&self) -> Result<(SearchForm, SearchQuery), AppError> {
        let bundle_id = match non_empty(&self.bundle) {
            None => None,
            Some(raw) => Some(
                raw.parse::<i64>()
                    .map_err(|_| AppError::bad_request("bundle must be an integer id"))?,
            ),
        };
        let limit = match non_empty(&self.limit) {
            None => DEFAULT_LIMIT,
            Some(raw) => raw
                .parse::<i32>()
                .ok()
                .filter(|n| (1..=MAX_LIMIT).contains(n))
                .ok_or_else(|| AppError::bad_request("limit must be between 1 and 100"))?,
        };
        let tags: Vec<String> = self
            .tags
            .split(',')
            .map(str::trim)
            .filter(|t| !t.is_empty())
            .map(str::to_owned)
            .collect();
        let after = match (
            non_empty(&self.after_rank),
            non_empty(&self.after_bundle),
            non_empty(&self.after_id),
        ) {
            (Some(rank), Some(bundle), Some(concept_id)) => Some(Cursor {
                rank: rank
                    .parse::<f32>()
                    .ok()
                    .filter(|r| r.is_finite())
                    .ok_or_else(|| AppError::bad_request("after_rank must be a finite number"))?,
                bundle_id: bundle
                    .parse()
                    .map_err(|_| AppError::bad_request("after_bundle must be an integer"))?,
                concept_id,
            }),
            (None, None, None) => None,
            _ => return Err(AppError::bad_request("an incomplete cursor was supplied")),
        };
        let mode = match self.mode.as_str() {
            "semantic" => "semantic",
            "hybrid" => "hybrid",
            _ => "lexical",
        };
        let form = SearchForm {
            q: self.q.trim().to_owned(),
            bundle: bundle_id.map(|b| b.to_string()).unwrap_or_default(),
            concept_type: self.concept_type.trim().to_owned(),
            tags: tags.join(", "),
            status: self.status.trim().to_owned(),
            trust: self.trust.trim().to_owned(),
            mode: mode.to_owned(),
            limit: limit.to_string(),
            limit_options: LIMIT_OPTIONS
                .iter()
                .map(|n| (n.to_string(), *n == limit))
                .collect(),
        };
        let query = SearchQuery {
            query: form.q.clone(),
            bundle_id,
            concept_type: non_empty(&form.concept_type),
            tags,
            status: non_empty(&form.status),
            trust_tier: non_empty(&form.trust),
            limit,
            after,
        };
        Ok((form, query))
    }

    fn is_append(&self) -> bool {
        matches!(self.append.as_str(), "1" | "true")
    }
}

fn search_url(base: &str, form: &SearchForm, cursor: Option<&Cursor>) -> String {
    let mut pairs: Vec<(String, String)> = Vec::new();
    if !form.q.is_empty() {
        pairs.push(("q".to_owned(), form.q.clone()));
    }
    pairs.extend(form.filter_pairs());
    if let Some(c) = cursor {
        pairs.push(("after_rank".to_owned(), c.rank.to_string()));
        pairs.push(("after_bundle".to_owned(), c.bundle_id.to_string()));
        pairs.push(("after_id".to_owned(), c.concept_id.clone()));
    }
    if pairs.is_empty() {
        return base.to_owned();
    }
    let query: Vec<String> = pairs
        .iter()
        .map(|(k, v)| format!("{k}={}", filters::percent_encode(v)))
        .collect();
    format!("{base}?{}", query.join("&"))
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

async fn bundle_names(app: &App) -> Result<BTreeMap<i64, String>, AppError> {
    Ok(app
        .db
        .bundles()
        .await?
        .into_iter()
        .map(|b| (b.id, b.name))
        .collect())
}

async fn dashboard(State(app): State<Shared>) -> PageResult {
    let (version, sql_version) = app.db.versions().await?;
    let health = HealthView::from_json(&app.db.health().await?, version, sql_version);
    let index = IndexView::from_json(&app.db.index_status().await?);
    let stats = app.db.catalog_stats().await?;
    let sync_log = app.db.sync_log(None, 10).await?;
    html(&DashboardPage {
        shell: Shell::new(&app, "Overview", "home"),
        health,
        index,
        stats,
        sync_log,
    })
}

/// The hits for a query, or its filter-only browse, with paging state.
async fn run_search(
    app: &App,
    form: &SearchForm,
    query: &SearchQuery,
    names: &BTreeMap<i64, String>,
) -> Result<ResultsView, AppError> {
    if query.query.is_empty() {
        if !query.has_filters() {
            return Ok(ResultsView::empty(
                "Search",
                "Type a query, or pick a filter to browse.",
            ));
        }
        return browse(app, form, query, names).await;
    }
    let mut notice = None;
    let mut degraded = false;
    let page = usize::try_from(query.limit).unwrap_or(usize::MAX);
    let mut hits = match (form.mode.as_str(), app.embedder.as_ref()) {
        ("lexical", _) => app.db.search(query, query.limit + 1).await?,
        (mode, Some(embedder)) => {
            let embedding = match embedder.embed(std::slice::from_ref(&query.query)).await {
                Ok(mut vectors) => vectors.pop(),
                Err(error) => {
                    eprintln!("pgokf-web: embedding the query failed: {error:#}");
                    None
                }
            };
            if let Some(embedding) = embedding {
                app.db
                    .search_with_embedding(query, &embedding, mode == "hybrid")
                    .await?
            } else {
                notice = Some(
                    "The embeddings service is unavailable; showing lexical results instead."
                        .to_owned(),
                );
                degraded = true;
                app.db.search(query, query.limit + 1).await?
            }
        }
        (_, None) => {
            notice = Some(
                "Semantic search is not configured on this server; showing lexical results."
                    .to_owned(),
            );
            degraded = true;
            app.db.search(query, query.limit + 1).await?
        }
    };
    // Lexical search asked for one row beyond the page: its presence is the
    // only reliable sign of a next page.
    let paginates = (form.mode == "lexical" || degraded) && hits.len() > page;
    hits.truncate(page);
    let cursor = paginates
        .then(|| {
            hits.last().map(|h| Cursor {
                rank: h.rank,
                bundle_id: h.bundle_id,
                concept_id: h.concept_id.clone(),
            })
        })
        .flatten();
    let count = hits.len();
    let summary = match (count, query.after.is_some(), paginates) {
        (0, _, _) => "No matches.".to_owned(),
        (n, false, true) => format!("First {n} matches, ranked by {} relevance.", form.mode),
        (n, false, false) => format!(
            "{n} match{}, ranked by {} relevance.",
            if n == 1 { "" } else { "es" },
            form.mode
        ),
        (n, true, _) => format!("{n} more."),
    };
    Ok(ResultsView {
        hits: hits
            .into_iter()
            .map(|h| HitView::from_hit(h, names))
            .collect(),
        heading: format!("Results for \u{201c}{}\u{201d}", form.q),
        summary,
        notice,
        next_url: cursor
            .as_ref()
            .map(|c| search_url("/search", form, Some(c))),
        next_partial_url: cursor
            .as_ref()
            .map(|c| format!("{}&append=1", search_url("/search/results", form, Some(c)))),
        browsing: false,
        degraded,
    })
}

/// Filter-only listing: every visible concept matching the filters, in
/// bundle and id order, paged by the same cursor shape (rank unused).
async fn browse(
    app: &App,
    form: &SearchForm,
    query: &SearchQuery,
    names: &BTreeMap<i64, String>,
) -> Result<ResultsView, AppError> {
    let page = usize::try_from(query.limit).unwrap_or(usize::MAX);
    let after = query
        .after
        .as_ref()
        .map(|c| (c.bundle_id, c.concept_id.as_str()));
    let mut rows = app
        .db
        .browse(query, after, i64::from(query.limit) + 1)
        .await?;
    let paginates = rows.len() > page;
    rows.truncate(page);
    let cursor = paginates
        .then(|| {
            rows.last().map(|c| Cursor {
                rank: 0.0,
                bundle_id: c.bundle_id,
                concept_id: c.concept_id.clone(),
            })
        })
        .flatten();
    let mut what: Vec<String> = Vec::new();
    if !form.concept_type.is_empty() {
        what.push(format!("type {}", form.concept_type));
    }
    if !form.tags.is_empty() {
        what.push(format!("tagged {}", form.tags));
    }
    if !form.status.is_empty() {
        what.push(format!("status {}", form.status));
    }
    if !form.trust.is_empty() {
        what.push(format!("trust tier {}", form.trust));
    }
    if let Some(name) = query.bundle_id.map(|id| bundle_name(names, id)) {
        what.push(format!("in {name}"));
    }
    let count = rows.len();
    let summary = match (count, query.after.is_some(), paginates) {
        (0, _, _) => "No concepts match these filters.".to_owned(),
        (n, false, true) => format!("First {n} concepts, by bundle and id."),
        (n, false, false) => format!("{n} concept{}.", if n == 1 { "" } else { "s" }),
        (n, true, _) => format!("{n} more."),
    };
    Ok(ResultsView {
        hits: rows
            .into_iter()
            .map(|c| HitView::from_summary(c, names))
            .collect(),
        heading: format!("Browsing concepts {}", what.join(", ")),
        summary,
        notice: None,
        next_url: cursor
            .as_ref()
            .map(|c| search_url("/search", form, Some(c))),
        next_partial_url: cursor
            .as_ref()
            .map(|c| format!("{}&append=1", search_url("/search/results", form, Some(c)))),
        browsing: true,
        degraded: false,
    })
}

async fn load_facets(
    app: &App,
    form: &SearchForm,
    query: &SearchQuery,
    names: &BTreeMap<i64, String>,
) -> Result<FacetsView, AppError> {
    let (types, tags, bundles, statuses, trust_tiers) = if query.query.is_empty() {
        // `search_facets` needs query text; before one is typed the sidebar
        // shows the whole visible catalog (or the chosen bundle).
        let b = query.bundle_id;
        (
            app.db.catalog_facets(b, "type").await?,
            app.db.catalog_facets(b, "tag").await?,
            Vec::new(),
            app.db.catalog_facets(b, "status").await?,
            app.db.catalog_facets(b, "trust_tier").await?,
        )
    } else {
        (
            app.db.facets(query, "type").await?,
            app.db.facets(query, "tag").await?,
            app.db.facets(query, "bundle").await?,
            app.db.facets(query, "status").await?,
            app.db.facets(query, "trust_tier").await?,
        )
    };
    Ok(FacetsView {
        types,
        tags: tags
            .into_iter()
            .take(24)
            .map(|f| FacetLink {
                href: search_url("/search", &form.with_tag(&f.value), None),
                active: form.has_tag(&f.value),
                label: f.value,
                count: f.count,
            })
            .collect(),
        bundles: bundles
            .into_iter()
            .map(|f| FacetLink {
                href: search_url("/search", &form.with_bundle(&f.value), None),
                active: form.bundle == f.value,
                label: f
                    .value
                    .parse::<i64>()
                    .ok()
                    .map_or_else(|| f.value.clone(), |id| bundle_name(names, id)),
                count: f.count,
            })
            .collect(),
        statuses: facet_options(statuses, &form.status),
        trust_tiers: facet_options(trust_tiers, &form.trust),
    })
}

async fn search_page(State(app): State<Shared>, Query(params): Query<SearchParams>) -> PageResult {
    let (form, query) = params.normalize()?;
    let health = app.db.health().await?;
    let backend_label = health["search_backend"]
        .as_str()
        .unwrap_or("native")
        .to_owned();
    let index = IndexView::from_json(&app.db.index_status().await?);
    let semantic_available =
        app.embedder.is_some() && index.embedding_available && index.embedded_rows > 0;
    let bundles = app.db.bundles().await?;
    let names: BTreeMap<i64, String> = bundles.iter().map(|b| (b.id, b.name.clone())).collect();
    let results = run_search(&app, &form, &query, &names).await?;
    let facets = load_facets(&app, &form, &query, &names).await?;
    let mut shell = Shell::new(&app, "Search", "search");
    shell.query.clone_from(&form.q);
    shell.filters = form.filter_pairs();
    html(&SearchPage {
        shell,
        form,
        bundles,
        facets,
        results,
        semantic_available,
        backend_label,
    })
}

/// The htmx partial: the result list (with its heading and, unless
/// appending, the refreshed facets swapped out of band). The pushed URL is
/// the full search URL, so reload, back, and share keep every filter.
async fn search_results(
    State(app): State<Shared>,
    Query(params): Query<SearchParams>,
) -> PageResult {
    let (form, query) = params.normalize()?;
    let append = params.is_append();
    let names = bundle_names(&app).await?;
    let results = run_search(&app, &form, &query, &names).await?;
    let facets = if append {
        FacetsView::empty()
    } else {
        load_facets(&app, &form, &query, &names).await?
    };
    let push_url = search_url("/search", &form, None);
    let mut response = html(&ResultsPartial {
        form,
        facets,
        results,
        append,
        oob: true,
    })?;
    if !append && let Ok(value) = HeaderValue::from_str(&push_url) {
        response.headers_mut().insert("HX-Push-Url", value);
    }
    Ok(response)
}

async fn bundles_page(State(app): State<Shared>) -> PageResult {
    let stats = app.db.catalog_stats().await?;
    html(&BundlesPage {
        shell: Shell::new(&app, "Bundles", "bundles"),
        stats,
    })
}

fn group_by_directory(concepts: Vec<ConceptSummary>) -> Vec<DirGroup> {
    let mut groups: BTreeMap<String, Vec<ConceptSummary>> = BTreeMap::new();
    for c in concepts {
        let directory = c
            .path
            .rsplit_once('/')
            .map_or(TOP_LEVEL_DIRECTORY, |(dir, _)| dir)
            .to_owned();
        groups.entry(directory).or_default().push(c);
    }
    // The bundle root comes first, then subdirectories alphabetically.
    let top = groups.remove(TOP_LEVEL_DIRECTORY);
    top.into_iter()
        .map(|concepts| (TOP_LEVEL_DIRECTORY.to_owned(), concepts))
        .chain(groups)
        .map(|(directory, concepts)| DirGroup {
            directory,
            concepts,
        })
        .collect()
}

/// Concepts listed per bundle page; larger bundles page by path.
const BUNDLE_PAGE: usize = 500;

#[derive(Debug, Default, Deserialize)]
struct BundleParams {
    /// Keyset cursor: list concepts whose path sorts after this one.
    #[serde(default)]
    after: String,
}

async fn bundle_page(
    State(app): State<Shared>,
    Path(id): Path<i64>,
    Query(params): Query<BundleParams>,
) -> PageResult {
    let bundle = app
        .db
        .bundle(id)
        .await?
        .ok_or_else(|| AppError::not_found("This bundle"))?;
    let after = non_empty(&params.after);
    let limit = i64::try_from(BUNDLE_PAGE).unwrap_or(i64::MAX);
    let mut concepts = app
        .db
        .bundle_concepts(id, after.as_deref(), limit + 1)
        .await?;
    let has_more = concepts.len() > BUNDLE_PAGE;
    concepts.truncate(BUNDLE_PAGE);
    let next_url = has_more
        .then(|| {
            concepts
                .last()
                .map(|c| format!("/bundles/{id}?after={}", filters::percent_encode(&c.path)))
        })
        .flatten();
    let shown = concepts.len();
    let groups = group_by_directory(concepts);
    let sync_log = app.db.sync_log(Some(id), 20).await?;
    let bundle_log = app.db.bundle_log(id, 50).await?;
    let title = bundle.name.clone();
    html(&BundlePage {
        shell: Shell::new(&app, &title, "bundles"),
        bundle,
        groups,
        page_size: BUNDLE_PAGE,
        shown,
        next_url,
        continued: after.is_some(),
        sync_log,
        bundle_log,
    })
}

#[derive(Debug, Default, Deserialize)]
struct ConceptParams {
    #[serde(default)]
    hops: String,
}

const DEFAULT_HOPS: i32 = 2;
const MAX_HOPS: i32 = 4;

fn parse_hops(raw: &str) -> i32 {
    raw.parse::<i32>()
        .ok()
        .filter(|h| (1..=MAX_HOPS).contains(h))
        .unwrap_or(DEFAULT_HOPS)
}

async fn concept_page(
    State(app): State<Shared>,
    Path((bundle_id, concept_id)): Path<(i64, String)>,
    Query(params): Query<ConceptParams>,
) -> PageResult {
    let hops = parse_hops(&params.hops);
    let c = app
        .db
        .concept(bundle_id, &concept_id)
        .await?
        .ok_or_else(|| AppError::not_found("This concept"))?;
    let (outgoing, incoming) = app.db.links(bundle_id, &concept_id).await?;
    let body_html = render_body(&c, &outgoing);
    let outgoing = group_links(&outgoing, LinkDirection::Outgoing, &concept_id);
    let incoming = group_links(&incoming, LinkDirection::Incoming, &concept_id);
    let neighbors = app.db.neighbors(bundle_id, &concept_id, hops).await?;
    let history = app.db.history(bundle_id, &concept_id, 50).await?;
    let names = bundle_names(&app).await?;
    let similar = app
        .db
        .similar(bundle_id, &concept_id, 8)
        .await?
        .into_iter()
        .map(|h| HitView::from_hit(h, &names))
        .collect();
    let graph_svg = neighbor_graph(&c, &neighbors);
    let metadata = c
        .metadata
        .iter()
        .map(|(key, value)| MetadataRow {
            key: key.clone(),
            value: serde_json::to_string_pretty(value).unwrap_or_default(),
        })
        .collect();
    let title = c.title.clone().unwrap_or_else(|| c.concept_id.clone());
    html(&ConceptPage {
        shell: Shell::new(&app, &title, "bundles"),
        self_href: concept_href(bundle_id, &concept_id),
        graph_url: graph_href(bundle_id, &concept_id),
        prov: ProvenanceView::from_concept(&c),
        c,
        body_html,
        metadata,
        outgoing,
        incoming,
        neighbors,
        graph_svg,
        hops,
        hops_options: (1..=MAX_HOPS).map(|n| (n, n == hops)).collect(),
        similar,
        history,
    })
}

/// The body as HTML: the exact stored Markdown when the catalog keeps
/// source (frontmatter removed, a leading H1 repeating the title dropped
/// since the header shows it, body links resolved through the catalog's
/// own link table), else the search-normalized text.
fn render_body(c: &ConceptDetail, outgoing: &[Link]) -> String {
    let title = c.title.as_deref().unwrap_or(&c.concept_id);
    let source = c
        .source
        .as_deref()
        .and_then(|bytes| std::str::from_utf8(bytes).ok());
    let Some(source) = source else {
        return markdown::render(&c.body_text);
    };
    let targets: HashMap<&str, String> = outgoing
        .iter()
        .filter(|l| l.resolved)
        .filter_map(|l| {
            Some((
                l.target_path.as_deref()?,
                concept_href(l.bundle_id, l.target_id.as_deref()?),
            ))
        })
        .collect();
    let resolver = Resolver::new(&c.path, targets);
    let body = markdown::strip_leading_title(markdown::strip_frontmatter(source), title);
    markdown::render_with_links(body, &|href| resolver.resolve(href))
}

fn concept_href(bundle_id: i64, concept_id: &str) -> String {
    format!("/concepts/{bundle_id}/{}", filters::encode_path(concept_id))
}

fn graph_href(bundle_id: i64, concept_id: &str) -> String {
    format!(
        "/api/graph/{bundle_id}/{}",
        filters::encode_path(concept_id)
    )
}

/// The neighborhood graph as the client draws it: nodes carry their page
/// and graph endpoints so the client never builds URLs from ids.
fn graph_json(bundle_id: i64, seed: &str, hops: i32, graph: &Graph) -> Value {
    let nodes: Vec<Value> = graph
        .nodes
        .iter()
        .map(|n| {
            serde_json::json!({
                "id": n.id,
                "title": n.title.as_deref().unwrap_or(&n.id),
                "type": n.concept_type,
                "path": n.path,
                "hops": n.hops,
                "href": concept_href(bundle_id, &n.id),
                "graph_href": graph_href(bundle_id, &n.id),
            })
        })
        .collect();
    serde_json::json!({
        "bundle_id": bundle_id,
        "seed": seed,
        "hops": hops,
        "nodes": nodes,
        "links": graph.links,
    })
}

/// Each neighbor's `path` is the chain of concept ids from the seed, so its
/// parent is the previous id in that chain (the seed itself is drawn as the
/// empty id at the center).
fn neighbor_edges(seed_id: &str, neighbors: &[Neighbor]) -> Vec<GraphEdge> {
    neighbors
        .iter()
        .map(|n| {
            let parent = n
                .path
                .iter()
                .rev()
                .nth(1)
                .filter(|p| p.as_str() != seed_id)
                .cloned()
                .unwrap_or_default();
            GraphEdge {
                from: parent,
                to: n.id.clone(),
            }
        })
        .collect()
}

fn neighbor_graph(seed: &ConceptDetail, neighbors: &[Neighbor]) -> String {
    let nodes: Vec<GraphNode> = neighbors
        .iter()
        .map(|n| GraphNode {
            id: n.id.clone(),
            label: n.title.clone().unwrap_or_else(|| n.id.clone()),
            hops: n.hops,
            href: concept_href(n.bundle_id, &n.id),
        })
        .collect();
    let edges = neighbor_edges(&seed.concept_id, neighbors);
    let placed = graph::layout(
        seed.title.as_deref().unwrap_or(&seed.concept_id),
        &concept_href(seed.bundle_id, &seed.concept_id),
        &nodes,
    );
    graph::svg(&placed, &edges)
}

/// `Content-Disposition` for a download named after the concept: an ASCII
/// fallback plus the RFC 5987 `filename*` form for anything else.
fn content_disposition(concept_id: &str) -> String {
    let base = concept_id.rsplit('/').next().unwrap_or("concept");
    let base = if base.is_empty() { "concept" } else { base };
    let ascii: String = base
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') {
                c
            } else {
                '_'
            }
        })
        .collect();
    format!(
        "attachment; filename=\"{ascii}.md\"; filename*=UTF-8''{}.md",
        filters::percent_encode(base)
    )
}

async fn concept_source(
    State(app): State<Shared>,
    Path((bundle_id, concept_id)): Path<(i64, String)>,
) -> PageResult {
    let bytes = match app.db.source_bytes(bundle_id, &concept_id).await {
        Ok(bytes) => bytes,
        // The extension raises invalid_parameter_value for a concept that
        // is missing, hidden, or stored without source.
        Err(error) if crate::db::classify(&error) == Failure::InvalidInput => {
            return Err(AppError::not_found("The stored source of this concept"));
        }
        Err(error) => return Err(error.into()),
    };
    Ok((
        [
            (
                header::CONTENT_TYPE,
                HeaderValue::from_static("text/markdown; charset=utf-8"),
            ),
            (
                header::CONTENT_DISPOSITION,
                HeaderValue::from_str(&content_disposition(&concept_id))
                    .unwrap_or_else(|_| HeaderValue::from_static("attachment")),
            ),
        ],
        bytes,
    )
        .into_response())
}

async fn status_page(State(app): State<Shared>) -> PageResult {
    let (version, sql_version) = app.db.versions().await?;
    let health = HealthView::from_json(&app.db.health().await?, version, sql_version);
    let index = IndexView::from_json(&app.db.index_status().await?);
    let config_json = serde_json::to_string_pretty(&app.db.config().await?).unwrap_or_default();
    let stale = app.db.stale().await?;
    let duplicates = app.db.duplicates().await?;
    let sync_log = app.db.sync_log(None, 50).await?;
    html(&StatusPage {
        shell: Shell::new(&app, "Operations", "status"),
        health,
        index,
        config_json,
        stale,
        duplicates,
        sync_log,
    })
}

async fn not_found() -> PageResult {
    Err(AppError::not_found("This page"))
}

// ---------------------------------------------------------------------------
// JSON API
// ---------------------------------------------------------------------------

async fn api_health(State(app): State<Shared>) -> Result<Json<Value>, AppError> {
    let mut health = app.db.health().await?;
    let (version, sql_version) = app.db.versions().await?;
    health["version"] = Value::String(version);
    health["sql_version"] = Value::String(sql_version);
    Ok(Json(health))
}

async fn api_bundles(State(app): State<Shared>) -> Result<Json<Value>, AppError> {
    let stats = app.db.catalog_stats().await?;
    Ok(Json(serde_json::to_value(stats).unwrap_or(Value::Null)))
}

async fn api_search(
    State(app): State<Shared>,
    Query(params): Query<SearchParams>,
) -> Result<Json<Value>, AppError> {
    let (form, query) = params.normalize()?;
    let names = bundle_names(&app).await?;
    let results = run_search(&app, &form, &query, &names).await?;
    if results.degraded {
        // A page can explain a fallback; an API caller asked for a mode and
        // must not get another one silently.
        return Err(AppError::bad_gateway(results.notice.unwrap_or_else(|| {
            "The requested search mode is unavailable.".to_owned()
        })));
    }
    let hits: Vec<Value> = results
        .hits
        .iter()
        .map(|h| {
            serde_json::json!({
                "bundle_id": h.bundle_id,
                "bundle_name": h.bundle_name,
                "concept_id": h.concept_id,
                "path": h.path,
                "title": h.title,
                "type": h.concept_type,
                "rank": h.rank,
                "headline_html": h.headline_html,
                "tags": h.tags,
            })
        })
        .collect();
    Ok(Json(serde_json::json!({
        "query": form.q,
        "mode": if results.browsing { "browse" } else { form.mode.as_str() },
        "hits": hits,
        "next": results.next_url,
        "notice": results.notice,
    })))
}

async fn api_graph(
    State(app): State<Shared>,
    Path((bundle_id, concept_id)): Path<(i64, String)>,
    Query(params): Query<ConceptParams>,
) -> Result<Json<Value>, AppError> {
    let hops = parse_hops(&params.hops);
    let graph = app.db.graph(bundle_id, &concept_id, hops).await?;
    if graph.nodes.is_empty() {
        return Err(AppError::not_found("This concept"));
    }
    Ok(Json(graph_json(bundle_id, &concept_id, hops, &graph)))
}

async fn api_concept(
    State(app): State<Shared>,
    Path((bundle_id, concept_id)): Path<(i64, String)>,
) -> Result<Json<Value>, AppError> {
    let c = app
        .db
        .concept(bundle_id, &concept_id)
        .await?
        .ok_or_else(|| AppError::not_found("This concept"))?;
    let (outgoing, incoming) = app.db.links(bundle_id, &concept_id).await?;
    let mut value = serde_json::to_value(&c).unwrap_or(Value::Null);
    value["links"] = serde_json::json!({ "outgoing": outgoing, "incoming": incoming });
    Ok(Json(value))
}

// ---------------------------------------------------------------------------
// Static assets (embedded at compile time; no filesystem at runtime)
// ---------------------------------------------------------------------------

const APP_CSS: &str = include_str!("../static/app.css");
const APP_JS: &str = include_str!("../static/app.js");
const BOOT_JS: &str = include_str!("../static/boot.js");
const HTMX_JS: &str = include_str!("../static/vendor/htmx.min.js");
const GRAPH_JS: &str = include_str!("../static/graph.js");
const FORCE_GRAPH_JS: &str = include_str!("../static/vendor/3d-force-graph.min.js");

async fn static_asset(Path(file): Path<String>) -> PageResult {
    let (content_type, body) = match file.as_str() {
        "app.css" => ("text/css; charset=utf-8", APP_CSS),
        "app.js" => ("text/javascript; charset=utf-8", APP_JS),
        "boot.js" => ("text/javascript; charset=utf-8", BOOT_JS),
        "htmx.min.js" => ("text/javascript; charset=utf-8", HTMX_JS),
        "graph.js" => ("text/javascript; charset=utf-8", GRAPH_JS),
        "3d-force-graph.min.js" => ("text/javascript; charset=utf-8", FORCE_GRAPH_JS),
        _ => return Err(AppError::not_found("This file")),
    };
    Ok((
        [
            (header::CONTENT_TYPE, HeaderValue::from_static(content_type)),
            // Safe to cache long: every reference carries the content hash.
            (
                header::CACHE_CONTROL,
                HeaderValue::from_static("public, max-age=31536000, immutable"),
            ),
        ],
        Body::from(body),
    )
        .into_response())
}

// ---------------------------------------------------------------------------
// Template filters
// ---------------------------------------------------------------------------

/// Custom Askama filters. Askama resolves `{{ x|name }}` to `filters::name`.
pub(crate) mod filters {
    use std::fmt::Display;

    /// Percent-encode a query-string value (RFC 3986 unreserved characters
    /// pass through; everything else, including `/`, is encoded).
    pub(crate) fn percent_encode(value: &str) -> String {
        let mut out = String::with_capacity(value.len());
        for byte in value.bytes() {
            match byte {
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                    out.push(byte as char);
                }
                _ => {
                    use std::fmt::Write as _;
                    let _ = write!(out, "%{byte:02X}");
                }
            }
        }
        out
    }

    /// Encode a concept id for a path: each `/`-separated segment is
    /// percent-encoded, the separators are kept.
    pub(crate) fn encode_path(value: &str) -> String {
        value
            .split('/')
            .map(percent_encode)
            .collect::<Vec<_>>()
            .join("/")
    }

    /// `{{ concept_id|urlencode_path }}`.
    #[allow(clippy::unnecessary_wraps)]
    pub(crate) fn urlencode_path<T: Display>(value: T) -> askama::Result<String> {
        Ok(encode_path(&value.to_string()))
    }

    /// `{{ iso|datetime }}`: an ISO-8601 UTC instant as `YYYY-MM-DD HH:MM UTC`.
    #[allow(clippy::unnecessary_wraps)]
    pub(crate) fn datetime<T: Display>(value: T) -> askama::Result<String> {
        Ok(format_datetime(&value.to_string()))
    }

    pub(crate) fn format_datetime(iso: &str) -> String {
        match (iso.get(0..10), iso.get(11..16)) {
            (Some(date), Some(time)) if iso.len() >= 16 => format!("{date} {time} UTC"),
            _ => iso.to_owned(),
        }
    }

    /// `{{ seconds|duration }}`: seconds as a compact human duration.
    // Askama hands filter arguments over by reference, hence `&i64`.
    #[allow(clippy::unnecessary_wraps, clippy::trivially_copy_pass_by_ref)]
    pub(crate) fn duration(value: &i64) -> askama::Result<String> {
        Ok(format_duration(*value))
    }

    pub(crate) fn format_duration(seconds: i64) -> String {
        let s = seconds.max(0);
        if s < 60 {
            format!("{s}s")
        } else if s < 3600 {
            format!("{}m", s / 60)
        } else if s < 86_400 {
            format!("{}h {}m", s / 3600, (s % 3600) / 60)
        } else {
            format!("{}d {}h", s / 86_400, (s % 86_400) / 3600)
        }
    }

    /// `{{ maybe_count|num }}`: an optional counter, `—` when absent.
    // Askama hands filter arguments over by reference.
    #[allow(
        clippy::unnecessary_wraps,
        clippy::ref_option,
        clippy::trivially_copy_pass_by_ref
    )]
    pub(crate) fn num(value: &Option<i32>) -> askama::Result<String> {
        Ok(value.map_or_else(|| "\u{2014}".to_owned(), |n| n.to_string()))
    }

    /// `{{ text|linkify }}`: escape the text and turn `http(s)://` URLs into
    /// links. Returns HTML, so the template marks it `|safe`.
    #[allow(clippy::unnecessary_wraps)]
    pub(crate) fn linkify<T: Display>(value: T) -> askama::Result<String> {
        let text = value.to_string();
        let mut out = String::with_capacity(text.len() + 32);
        for (index, word) in text.split(' ').enumerate() {
            if index > 0 {
                out.push(' ');
            }
            let escaped = crate::markdown::escape(word);
            if word.starts_with("http://") || word.starts_with("https://") {
                use std::fmt::Write as _;
                let _ = write!(
                    out,
                    "<a href=\"{escaped}\" rel=\"noopener noreferrer\">{escaped}</a>"
                );
            } else {
                out.push_str(&escaped);
            }
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn params(pairs: &[(&str, &str)]) -> SearchParams {
        let mut p = SearchParams::default();
        for (k, v) in pairs {
            match *k {
                "q" => p.q = (*v).to_owned(),
                "bundle" => p.bundle = (*v).to_owned(),
                "tags" => p.tags = (*v).to_owned(),
                "limit" => p.limit = (*v).to_owned(),
                "mode" => p.mode = (*v).to_owned(),
                "after_rank" => p.after_rank = (*v).to_owned(),
                "after_bundle" => p.after_bundle = (*v).to_owned(),
                "after_id" => p.after_id = (*v).to_owned(),
                _ => {}
            }
        }
        p
    }

    fn form(q: &str, tags: &str, bundle: &str) -> SearchForm {
        SearchForm {
            q: q.to_owned(),
            bundle: bundle.to_owned(),
            concept_type: String::new(),
            tags: tags.to_owned(),
            status: String::new(),
            trust: String::new(),
            mode: "lexical".to_owned(),
            limit: "20".to_owned(),
            limit_options: Vec::new(),
        }
    }

    fn link(target: Option<&str>, path: Option<&str>, text: &str, external: bool) -> Link {
        Link {
            bundle_id: 1,
            source_id: "a".to_owned(),
            target_id: target.map(str::to_owned),
            text: Some(text.to_owned()),
            target_path: path.map(str::to_owned),
            kind: "inline".to_owned(),
            relation: DEFAULT_RELATION.to_owned(),
            resolved: target.is_some(),
            is_external: external,
            counterpart_title: target.map(|t| format!("Title of {t}")),
        }
    }

    #[test]
    fn normalize_defaults_limit_and_mode_and_splits_tags() {
        // Arrange
        let p = params(&[("q", " failover "), ("tags", "a, b ,,c")]);

        // Act
        let (form, query) = p.normalize().ok().expect("valid params");

        // Assert
        assert_eq!(query.query, "failover");
        assert_eq!(query.limit, DEFAULT_LIMIT);
        assert_eq!(query.tags, vec!["a", "b", "c"]);
        assert_eq!(form.mode, "lexical");
        assert!(query.after.is_none());
    }

    #[test]
    fn normalize_rejects_partial_cursors_bad_limits_and_non_finite_ranks() {
        // Arrange
        let partial = params(&[("q", "x"), ("after_rank", "0.5")]);
        let oversized = params(&[("q", "x"), ("limit", "500")]);
        let nan = params(&[
            ("q", "x"),
            ("after_rank", "NaN"),
            ("after_bundle", "1"),
            ("after_id", "a"),
        ]);
        let huge = params(&[
            ("q", "x"),
            ("after_rank", "1e40"),
            ("after_bundle", "1"),
            ("after_id", "a"),
        ]);

        // Act & Assert
        assert!(partial.normalize().is_err());
        assert!(oversized.normalize().is_err());
        assert!(nan.normalize().is_err());
        assert!(huge.normalize().is_err());
    }

    #[test]
    fn normalize_accepts_a_complete_cursor() {
        // Arrange
        let p = params(&[
            ("q", "x"),
            ("after_rank", "0.25"),
            ("after_bundle", "3"),
            ("after_id", "runbooks/a"),
        ]);

        // Act
        let (_, query) = p.normalize().ok().expect("valid params");

        // Assert
        let cursor = query.after.expect("cursor parsed");
        assert_eq!(cursor.bundle_id, 3);
        assert_eq!(cursor.concept_id, "runbooks/a");
    }

    #[test]
    fn search_url_round_trips_the_form_and_encodes_values() {
        // Arrange
        let f = form("a b&c", "x, y", "2");

        // Act
        let url = search_url("/search", &f, None);

        // Assert
        assert_eq!(url, "/search?q=a%20b%26c&bundle=2&tags=x%2C%20y");
        assert_eq!(
            search_url(
                "/search",
                &SearchForm {
                    mode: "hybrid".to_owned(),
                    limit: "50".to_owned(),
                    ..f
                },
                None
            ),
            "/search?q=a%20b%26c&bundle=2&tags=x%2C%20y&mode=hybrid&limit=50"
        );
    }

    #[test]
    fn with_tag_appends_once_and_keeps_the_other_filters() {
        // Arrange
        let f = form("q", "x", "2");

        // Act
        let once = f.with_tag("y");
        let twice = once.with_tag("y");

        // Assert
        assert_eq!(once.tags, "x, y");
        assert_eq!(twice.tags, "x, y");
        assert_eq!(once.bundle, "2");
        assert!(once.has_tag("y"));
        assert!(!f.has_tag("y"));
    }

    #[test]
    fn facet_options_keep_an_unmatched_current_choice_selectable() {
        // Arrange
        let buckets = vec![Facet {
            value: "stable".to_owned(),
            count: 3,
        }];

        // Act
        let matched = facet_options(buckets.clone(), "stable");
        let unmatched = facet_options(buckets, "deprecated");

        // Assert
        assert_eq!(matched.len(), 1);
        assert!(matched[0].selected);
        assert_eq!(matched[0].label, "stable (3)");
        assert_eq!(unmatched.len(), 2);
        assert!(unmatched[1].selected);
        assert_eq!(unmatched[1].value, "deprecated");
    }

    #[test]
    fn group_links_folds_repeats_and_labels_by_counterpart_title() {
        // Arrange
        let links = vec![
            link(Some("b"), Some("b.md"), "see b", false),
            link(Some("b"), Some("b.md"), "b again", false),
            link(None, None, "https://x.example/", true),
            link(None, Some("missing.md"), "gone", false),
            link(Some("a"), Some("a.md"), "self", false),
        ];

        // Act
        let groups = group_links(&links, LinkDirection::Outgoing, "a");

        // Assert
        assert_eq!(groups.len(), 4);
        assert_eq!(groups[0].label, "Title of b");
        assert_eq!(groups[0].count, 2);
        assert_eq!(groups[0].href.as_deref(), Some("/concepts/1/b"));
        assert!(groups[1].is_external());
        assert_eq!(groups[2].state, LinkState::Unresolved);
        assert_eq!(groups[2].detail.as_deref(), Some("missing.md"));
        assert!(groups[3].is_self);
    }

    #[test]
    fn neighbor_edges_link_each_node_to_the_previous_step_of_its_path() {
        // Arrange
        let mk = |id: &str, hops: i32, path: &[&str]| Neighbor {
            bundle_id: 1,
            id: id.to_owned(),
            title: None,
            hops,
            path: path.iter().map(|p| (*p).to_owned()).collect(),
        };
        let neighbors = vec![mk("b", 1, &["seed", "b"]), mk("c", 2, &["seed", "b", "c"])];

        // Act
        let edges = neighbor_edges("seed", &neighbors);

        // Assert
        assert_eq!(
            edges,
            vec![
                GraphEdge {
                    from: String::new(),
                    to: "b".to_owned()
                },
                GraphEdge {
                    from: "b".to_owned(),
                    to: "c".to_owned()
                },
            ]
        );
    }

    #[test]
    fn actor_display_reads_strings_and_the_mapping_form() {
        // Arrange
        let plain = serde_json::json!("human:alice");
        let mapping = serde_json::json!({"id": "agent:reviewer", "display": "Reviewer"});
        let opaque = serde_json::json!({"team": "platform"});

        // Act & Assert
        assert_eq!(actor_display(&plain).as_deref(), Some("human:alice"));
        assert_eq!(actor_display(&mapping).as_deref(), Some("agent:reviewer"));
        assert_eq!(
            actor_display(&opaque).as_deref(),
            Some("{\"team\":\"platform\"}")
        );
        assert_eq!(actor_display(&Value::Null), None);
    }

    #[test]
    fn parse_hops_clamps_to_the_supported_range() {
        // Arrange & Act & Assert
        assert_eq!(parse_hops("3"), 3);
        assert_eq!(parse_hops("0"), DEFAULT_HOPS);
        assert_eq!(parse_hops("9"), DEFAULT_HOPS);
        assert_eq!(parse_hops("x"), DEFAULT_HOPS);
    }

    #[test]
    fn health_view_tolerates_missing_keys() {
        // Arrange
        let value = serde_json::json!({ "ok": true });

        // Act
        let view = HealthView::from_json(&value, "1".to_owned(), "1".to_owned());
        let index = IndexView::from_json(&serde_json::json!({}));

        // Assert
        assert!(view.ok);
        assert_eq!(view.search_backend, "native");
        assert_eq!(view.bundle_count, 0);
        assert_eq!(index.embedding_coverage, "0%");
        assert_eq!(index.embedding_coverage_pct, 0);
    }

    #[test]
    fn content_disposition_offers_an_ascii_fallback_and_an_encoded_name() {
        // Arrange & Act
        let plain = content_disposition("runbooks/database-failover");
        let odd = content_disposition("notes/caf\u{e9} \"quoted\"");

        // Assert
        assert_eq!(
            plain,
            "attachment; filename=\"database-failover.md\"; filename*=UTF-8''database-failover.md"
        );
        assert_eq!(
            odd,
            "attachment; filename=\"caf___quoted_.md\"; filename*=UTF-8''caf%C3%A9%20%22quoted%22.md"
        );
    }

    #[test]
    fn group_by_directory_labels_root_concepts_top_level() {
        // Arrange
        let mk = |path: &str| ConceptSummary {
            bundle_id: 1,
            concept_id: path.trim_end_matches(".md").to_owned(),
            path: path.to_owned(),
            concept_type: None,
            title: None,
            description: None,
            tags: Vec::new(),
            modified_at: None,
        };

        // Act
        let groups = group_by_directory(vec![
            mk("index.md"),
            mk("runbooks/a.md"),
            mk("runbooks/b.md"),
        ]);

        // Assert
        assert_eq!(groups[0].directory, TOP_LEVEL_DIRECTORY);
        assert_eq!(groups[1].directory, "runbooks");
        assert_eq!(groups[1].concepts.len(), 2);
    }

    #[test]
    fn filters_format_instants_durations_and_optional_counts() {
        // Arrange & Act & Assert
        assert_eq!(
            filters::format_datetime("2026-09-06T15:41:40Z"),
            "2026-09-06 15:41 UTC"
        );
        assert_eq!(filters::format_datetime("never"), "never");
        assert_eq!(filters::format_duration(45), "45s");
        assert_eq!(filters::format_duration(243), "4m");
        assert_eq!(filters::format_duration(3_900), "1h 5m");
        assert_eq!(filters::format_duration(200_000), "2d 7h");
        assert_eq!(filters::num(&Some(3)).expect("renders"), "3");
        assert_eq!(filters::num(&None).expect("renders"), "\u{2014}");
    }

    #[test]
    fn filters_encode_paths_per_segment_and_linkify_escapes() {
        // Arrange & Act
        let path = filters::encode_path("a b/c&d");
        let link =
            filters::linkify("see https://x.example/?a=1&b=2 <now>").expect("linkify renders");

        // Assert
        assert_eq!(path, "a%20b/c%26d");
        assert!(link.contains("<a href=\"https://x.example/?a=1&amp;b=2\""));
        assert!(link.contains("&lt;now&gt;"));
    }
}
