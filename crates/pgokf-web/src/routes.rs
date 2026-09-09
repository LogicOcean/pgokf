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
use axum::extract::{
    DefaultBodyLimit, Form, FromRequestParts, Multipart, Path, Query, Request, State,
};
use axum::http::request::Parts;
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use pgokf_companion::embeddings::EmbeddingsClient;
use pgokf_companion::mcp_token::{Role as McpRole, valid_token_name};
use pgokf_workspace::{
    BuildOptions, Component, ConceptRecord, ConceptRef, CustomHarness, Profile, Selection, Shape,
    Target,
};
use serde::Deserialize;
use serde_json::Value;
use tower::limit::ConcurrencyLimitLayer;

use crate::auth::{Admission, Authenticator, Mode, Principal, Role, Session, cookie_header};
use crate::db::{
    AdminBundle, BundleFile, BundleInfo, BundleLogEntry, BundleStat, ConceptDetail, ConceptSummary,
    Cursor, Db, DuplicateGroup, Facet, Failure, Graph, Hit, Link, Neighbor, PackageInfo,
    PersonalItem, ResourceInfo, ReviewItem, SearchQuery, StaleConcept, SyncLogEntry, SyncOutcome,
    Version,
};
use crate::graph::{GraphEdge, GraphNode};
use crate::links::Resolver;
use crate::mcp_tokens::{McpToken, McpTokens, Minted};
use crate::oidc::OidcAuth;
use crate::provider::{ProviderDraft, SecretChange};
use crate::provider_settings::{ProviderKind, ProviderSettings};
use crate::store::DocumentStore;
use crate::user_store::PeopleQuery;
use crate::{graph, markdown};
use pgokf_companion::documents::{Document, now_iso};
use pgokf_workspace::drop_packaged_resources;

/// Shared application state.
pub(crate) struct App {
    pub db: Db,
    /// The writer connection the human workflow uses; `None` keeps the UI
    /// read-only.
    pub writer: Option<Db>,
    /// The MCP bearer tokens, minted and revoked on the Admin page; `None`
    /// without a writer connection, since they live in the catalog.
    pub mcp_tokens: Option<McpTokens>,
    pub auth: Authenticator,
    /// Reverse proxies whose `X-Forwarded-For` is believed, so the sign-in
    /// throttle keys on the real client behind them and not on the one proxy
    /// every request arrives from.
    pub trusted_proxies: crate::auth::TrustedProxies,
    /// Bundle rebuilds run one at a time: a content resync is a full
    /// snapshot, so two interleaved ones could lose each other's change.
    pub rebuilds: tokio::sync::Mutex<()>,
    /// Plugin builds run a few at a time: each holds a pooled reader for
    /// its whole run, so unbounded they take every connection and the rest
    /// of the site answers "the catalog is busy".
    pub builds: tokio::sync::Semaphore,
    /// Where directory bundles are reachable from this process, if at all.
    pub stores: crate::store::Stores,
    pub embedder: Option<EmbeddingsClient>,
    pub catalog_name: String,
    pub tenant: Option<String>,
    /// The library version seen at startup, for the footer.
    pub version: String,
}

type Shared = Arc<App>;

/// Wall-clock bound on one request, covering every statement it issues.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// How many of the catalog's most used tags the builder offers as chips.
const CHIP_TAGS: usize = 14;

/// The most concepts the file picker lists for one bundle.
const TREE_CAP: i64 = 5000;

/// Requests handled at once; the rest queue (and time out) rather than
/// piling onto the connection pool.
const MAX_IN_FLIGHT: usize = 64;
/// Plugin builds allowed to run at once. Each holds a pooled reader
/// connection for its whole run - the selection, up to 500 audited source
/// reads, and an in-memory deflate - so unbounded, a handful of them take
/// every connection and the rest of the site answers "the catalog is busy".
pub(crate) const MAX_PLUGIN_BUILDS: usize = 2;

/// Build the router.
pub(crate) fn router(app: Shared) -> Router {
    let api = Router::new()
        .route("/health", get(api_health))
        .route("/bundles", get(api_bundles))
        .route("/bundles/{id}/tree", get(api_bundle_tree))
        .route("/search", get(api_search))
        .route("/concepts/{bundle_id}/{*concept_id}", get(api_concept))
        .route("/graph", get(api_catalog_graph))
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
        .route("/resource/{bundle_id}/{*concept_id}", get(concept_resource))
        .route("/graph", get(graph_page))
        .route("/plugins", get(plugins_page))
        .route("/plugins/preview", get(plugins_preview))
        .route("/plugins/build.zip", get(plugins_zip))
        .route("/login", get(login_page).post(login_submit))
        .route("/login/provider", get(login_provider))
        .route("/auth/callback", get(auth_callback))
        .route("/logout", post(logout))
        .route("/profile", get(profile_page))
        .route("/profile/password", post(profile_password))
        .route("/profile/sessions", post(profile_sessions_end))
        .route("/admin", get(admin_home))
        .route("/admin/people", get(admin_people_page))
        .route("/admin/users", post(admin_users))
        .route("/admin/sessions", post(admin_sessions_end))
        .route("/admin/tokens", get(admin_tokens_page))
        .route("/admin/mcp-tokens", post(admin_mcp_tokens))
        .route(
            "/admin/providers",
            get(admin_providers_page).post(admin_provider),
        )
        .route("/admin/providers/new", get(admin_provider_new_page))
        .route("/admin/providers/{id}", get(admin_provider_edit_page))
        .route(
            "/admin/bundles",
            get(admin_bundles_page).post(admin_bundles),
        )
        .route("/admin/settings", get(admin_settings_page))
        .route("/review", get(review_page))
        .route("/review/{bundle_id}/{*concept_id}", post(concept_review))
        .merge(
            Router::new()
                .route("/upload", get(upload_page).post(upload_submit))
                .route(
                    "/edit/{bundle_id}/{*concept_id}",
                    get(edit_page).post(edit_submit),
                )
                .route("/edit-check/{bundle_id}/{*concept_id}", post(edit_check))
                .layer(DefaultBodyLimit::max(UPLOAD_LIMIT)),
        )
        .route("/static/{file}", get(static_asset))
        .nest("/api", api)
        .fallback(not_found)
        .layer(ConcurrencyLimitLayer::new(MAX_IN_FLIGHT))
        .layer(middleware::from_fn(request_timeout))
        .layer(middleware::from_fn(shape_errors))
        .layer(middleware::from_fn_with_state(app.clone(), authenticate))
        .layer(middleware::from_fn(same_origin_writes))
        // Outermost, because a layer wraps everything added before it: the
        // 401s, the cross-site refusal and the sign-in redirect are answers
        // this server gives too, and they carry the same headers as a page.
        .layer(middleware::from_fn(security_headers))
        .with_state(app)
}

/// The most bytes one upload request may carry.
const UPLOAD_LIMIT: usize = 32 * 1024 * 1024;
/// The most documents one upload may carry.
const MAX_UPLOAD_FILES: usize = 200;
/// The most items the review queue lists.
const REVIEW_LIMIT: i64 = 500;

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

/// Resolve who is asking and attach the [`Session`] to the request. Once
/// people are identified at all (any mode but `none`), a request from
/// nobody reaches nothing but the login page, the static assets, and the
/// health probe: signing out ends access to the site.
async fn authenticate(State(app): State<Shared>, mut request: Request, next: Next) -> Response {
    let peer = Session::peer_of(request.extensions());
    // Static assets need no identity, so they never touch the identity
    // store; everything else is resolved, and a store that cannot answer is
    // an honest 503 - not a request quietly treated as anonymous.
    let path = request.uri().path().to_owned();
    let principal = if path.starts_with("/static/") {
        None
    } else {
        match app.auth.identify(request.headers(), peer).await {
            Ok(principal) => principal,
            // The paths open to anyone grant nothing, so they proceed as
            // anonymous through an outage: that is what lets sign-out still
            // clear the cookie (and fail loudly on the deletion), sign-in
            // still render, and the health probe still answer.
            Err(error) if open_to_anyone(&path) => {
                eprintln!("pgokf-web: identity lookup failed on an open path: {error:#}");
                None
            }
            Err(error) => return identity_unavailable(error, &path),
        }
    };
    let mode = app.auth.mode();
    if mode != Mode::None && principal.is_none() && !open_to_anyone(request.uri().path()) {
        let path = request.uri().path();
        if path == "/api" || path.starts_with("/api/") {
            let body = serde_json::json!({
                "error": { "status": 401, "message": "Sign in required." }
            });
            return (StatusCode::UNAUTHORIZED, Json(body)).into_response();
        }
        let next = request.uri().path_and_query().map_or("/", |pq| pq.as_str());
        return if mode.is_local_session() {
            AppError::sign_in(next).into_response()
        } else {
            AppError::forbidden_message(
                "Sign in through the site's identity provider; this server believes only its proxy.",
            )
            .into_response()
        };
    }
    request.extensions_mut().insert(Session {
        principal,
        mode,
        peer,
    });
    next.run(request).await
}

/// The response when the identity store could not be consulted for a path
/// that needs an identity: the failure classified as every other catalog
/// failure is (a busy pool is a 503), and shaped for the caller - the JSON
/// envelope under `/api`, since this middleware sits outside the layer that
/// would otherwise shape it.
fn identity_unavailable(error: anyhow::Error, path: &str) -> Response {
    let failure = AppError::from(error);
    if path == "/api" || path.starts_with("/api/") {
        let body = serde_json::json!({
            "error": { "status": failure.status.as_u16(), "message": failure.message }
        });
        return (failure.status, Json(body)).into_response();
    }
    failure.into_response()
}

/// The paths a person who is not signed in may still reach.
fn open_to_anyone(path: &str) -> bool {
    matches!(
        path,
        "/login" | "/login/provider" | "/logout" | "/auth/callback" | "/api/health"
    ) || path.starts_with("/static/")
}

/// Refuse state-changing requests that a browser sends from another site:
/// the session cookie is `SameSite=Lax`, and this is the second lock.
async fn same_origin_writes(request: Request, next: Next) -> Response {
    let mutating = matches!(
        *request.method(),
        Method::POST | Method::PUT | Method::PATCH | Method::DELETE
    );
    if mutating && !same_origin(request.headers()) {
        return AppError::forbidden_message("Cross-site requests are refused.").into_response();
    }
    next.run(request).await
}

/// Whether the browser says the request came from this site: the fetch
/// metadata header when present, else the `Origin` against the `Host`. A
/// request with neither (a non-browser client) passes.
fn same_origin(headers: &HeaderMap) -> bool {
    match headers.get("sec-fetch-site").and_then(|v| v.to_str().ok()) {
        Some("same-origin" | "none") => true,
        Some(_) => false,
        None => match (
            headers.get(header::ORIGIN).and_then(|v| v.to_str().ok()),
            headers.get(header::HOST).and_then(|v| v.to_str().ok()),
        ) {
            (Some(origin), Some(host)) => origin
                .split_once("://")
                .is_some_and(|(_, rest)| rest.eq_ignore_ascii_case(host)),
            (Some(_), None) => false,
            (None, _) => true,
        },
    }
}

impl<S: Send + Sync> FromRequestParts<S> for Session {
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        Ok(parts
            .extensions
            .get::<Session>()
            .cloned()
            .unwrap_or_else(|| Session::anonymous(Mode::None)))
    }
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
        AppError {
            status,
            message,
            location: None,
        }
        .into_response()
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
    /// A redirect instead of an error page (signing in first).
    location: Option<String>,
}

impl AppError {
    fn with(status: StatusCode, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
            location: None,
        }
    }

    fn not_found(what: &str) -> Self {
        Self::with(
            StatusCode::NOT_FOUND,
            format!("{what} is not visible to this session or does not exist."),
        )
    }

    fn bad_request(message: impl Into<String>) -> Self {
        Self::with(StatusCode::BAD_REQUEST, message)
    }

    fn timeout() -> Self {
        Self::with(
            StatusCode::GATEWAY_TIMEOUT,
            "The catalog took too long to answer; try a narrower query.",
        )
    }

    fn bad_gateway(message: impl Into<String>) -> Self {
        Self::with(StatusCode::BAD_GATEWAY, message)
    }

    /// The role the page needs is not held.
    fn forbidden(role: Role) -> Self {
        Self::with(
            StatusCode::FORBIDDEN,
            format!(
                "This needs the {} role; yours does not include it.",
                role.id()
            ),
        )
    }

    fn forbidden_message(message: impl Into<String>) -> Self {
        Self::with(StatusCode::FORBIDDEN, message)
    }

    /// Nobody is signed in and there is a login page: go there, and come
    /// back to `next` afterwards.
    fn sign_in(next: &str) -> Self {
        Self {
            status: StatusCode::SEE_OTHER,
            message: "Sign in to continue.".to_owned(),
            location: Some(format!("/login?next={}", filters::percent_encode(next))),
        }
    }

    fn unavailable(message: impl Into<String>) -> Self {
        Self::with(StatusCode::SERVICE_UNAVAILABLE, message)
    }

    fn status(&self) -> StatusCode {
        self.status
    }

    fn message(&self) -> &str {
        &self.message
    }
}

impl From<anyhow::Error> for AppError {
    fn from(error: anyhow::Error) -> Self {
        // The operator sees the cause in the log; the page sees a summary.
        eprintln!("pgokf-web: request failed: {error:#}");
        match crate::db::classify(&error) {
            Failure::Busy => Self::with(
                StatusCode::SERVICE_UNAVAILABLE,
                "The catalog is busy; try again in a moment.",
            ),
            Failure::Timeout => Self::timeout(),
            Failure::InvalidInput => Self::bad_request(
                crate::db::db_message(&error)
                    .unwrap_or_else(|| "The catalog rejected a request value.".to_owned()),
            ),
            Failure::Other => Self::with(
                StatusCode::INTERNAL_SERVER_ERROR,
                "The catalog query failed; the server log has the cause.",
            ),
        }
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        if let Some(location) = &self.location {
            return redirect(location);
        }
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

/// A `303 See Other` to a page of this site.
fn redirect(to: &str) -> Response {
    let location = HeaderValue::from_str(to).unwrap_or_else(|_| HeaderValue::from_static("/"));
    (StatusCode::SEE_OTHER, [(header::LOCATION, location)]).into_response()
}

/// A `next` parameter that stays on this site: one path, never a URL. A
/// second slash or a backslash (which browsers read as a slash) would make
/// it a link to another host, and control characters have no place in it.
fn safe_next(next: &str) -> String {
    let next = next.trim();
    let plain = next.chars().all(|c| !c.is_control() && c != '\\');
    if plain && next.starts_with('/') && !next.starts_with("//") && !next.contains("://") {
        next.to_owned()
    } else {
        "/".to_owned()
    }
}

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
// The flags mirror the header's toggles one to one.
#[allow(clippy::struct_excessive_bools)]
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
    /// The signed-in person, when there is one.
    pub user: Option<UserView>,
    /// Where to sign in, when the server has a login page and nobody is.
    pub signin_url: Option<String>,
    /// Whether the human workflow is on (a writer connection is set).
    pub workflow: bool,
    pub can_upload: bool,
    pub can_review: bool,
    pub can_admin: bool,
    /// Whether this server holds the session itself (`users` or `oidc`).
    pub can_sign_out: bool,
}

/// The signed-in person as the header shows them.
pub(crate) struct UserView {
    pub display: String,
    pub role: String,
}

impl Shell {
    fn new(app: &App, session: &Session, page_title: &str, nav: &'static str) -> Self {
        let workflow = app.writer.is_some();
        Self {
            page_title: page_title.to_owned(),
            catalog_name: app.catalog_name.clone(),
            nav: nav.to_owned(),
            query: String::new(),
            filters: Vec::new(),
            tenant: app.tenant.clone(),
            version: app.version.clone(),
            asset_version: ASSET_VERSION.clone(),
            user: session.principal.as_ref().map(|p| UserView {
                display: p.display.clone(),
                role: p.role.id().to_owned(),
            }),
            signin_url: (session.mode.is_local_session() && session.principal.is_none())
                .then(|| "/login".to_owned()),
            workflow,
            can_upload: workflow && session.allows(Role::Uploader),
            can_review: workflow && session.allows(Role::Approver),
            can_admin: session.allows(Role::Admin),
            can_sign_out: session.mode.is_local_session() && session.principal.is_some(),
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
            user: None,
            signin_url: None,
            workflow: false,
            can_upload: false,
            can_review: false,
            can_admin: false,
            can_sign_out: false,
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

/// A target profile as the builder page lists it.
/// One kind of tree the builder can make (the first choice on the page).
pub(crate) struct KindView {
    pub id: String,
    pub label: String,
    pub description: String,
    pub selected: bool,
}

/// One registry agent, listed under its kind for the agent chooser.
pub(crate) struct AgentView {
    pub id: String,
    pub label: String,
    pub kind: String,
    /// Where the tree lands, as a short path.
    pub root: String,
    pub notes: String,
}

/// The builder form's state, echoed back into the fields.
#[derive(Debug, Clone)]
// The flags mirror the form's checkboxes one to one.
#[allow(clippy::struct_excessive_bools)]
pub(crate) struct PluginForm {
    /// The kind of tree (a `Shape` id).
    pub kind: String,
    /// The agent as the chooser shows it: a registry label or the typed name.
    pub agent: String,
    /// Where a custom skills agent reads skills from.
    pub skills_dir: String,
    /// `true` when the agent is one the user added rather than a registry one.
    pub custom: bool,
    /// One line about the chosen agent (its note, or what adding one means).
    pub agent_note: String,
    /// Why the chosen agent cannot be built for (a bad skills directory);
    /// shown on the page, refused by the download.
    pub agent_problem: Option<String>,
    /// Whether the selection starts from everything in scope (a rule that
    /// stays in step with the catalog) rather than from ticked files only.
    pub all: bool,
    /// The selection in words, for the page before any preview.
    pub selection_summary: String,
    pub name: String,
    pub title: String,
    pub bundle: String,
    pub types: String,
    pub tags: String,
    pub ids: String,
    /// One `bundle_id:concept_id` per line.
    pub picks: String,
    pub q: String,
    pub verified: bool,
    pub limit: String,
    pub base_model: String,
    pub with_mcp: bool,
    pub with_guide: bool,
    pub with_tools: bool,
    pub mcp_command: String,
    pub mcp_url: String,
    pub web_url: String,
    /// `true` once any selector is set.
    pub has_selection: bool,
    /// A malformed picks line, shown with the preview; the download refuses it.
    pub pick_problem: Option<String>,
    /// The same request as a query string, for the download link and the
    /// preview partial.
    pub query_string: String,
}

impl PluginForm {
    /// The first thing wrong with the form that the page shows and the
    /// download refuses: an agent that cannot be built for, then a
    /// malformed ticked-files line.
    fn problems(&self) -> Option<String> {
        self.agent_problem
            .clone()
            .or_else(|| self.pick_problem.clone())
    }
}

/// What a selection resolves to, before anything is downloaded.
pub(crate) struct PluginPreview {
    pub description: String,
    pub concepts: Vec<ConceptRecord>,
    pub truncated: bool,
    pub root: String,
    pub index_file: String,
    pub file_paths: Vec<String>,
    /// Where the MCP configuration lands for this target, when included.
    pub mcp_note: Option<String>,
    pub mcp_call: String,
    pub download_url: String,
    /// How many of the concepts are skill packages copied whole.
    pub package_count: usize,
    /// The chosen target's display name.
    pub target_label: String,
    /// Where the unpacked zip goes for this target.
    pub install_note: String,
}

/// One custom-metadata row, value pretty-printed.
pub(crate) struct MetadataRow {
    pub key: String,
    pub value: String,
}

/// The link relation OKF assigns to an ordinary Markdown link; it carries no
/// information on the links tab, so only other relations are shown.
const DEFAULT_RELATION: &str = "reference";

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
    /// A one-line outcome of the action that led here.
    notice: Option<String>,
    concepts: Vec<ConceptSummary>,
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
    /// The skill package this concept is the manifest of, if any.
    package: Option<PackageInfo>,
    /// The package resource this concept is, if any.
    resource: Option<ResourceInfo>,
    /// A one-line outcome of the action that led here.
    notice: Option<String>,
    /// The human workflow's view of this concept for the signed-in person.
    workflow: ConceptWorkflow,
}

/// What the signed-in person may do to a concept here, and why not.
#[derive(Debug, Clone, Default)]
pub(crate) struct ConceptWorkflow {
    pub can_edit: bool,
    pub can_review: bool,
    /// Why the document cannot be changed from the UI, for people who
    /// otherwise could.
    pub blocker: Option<String>,
}

#[derive(Template)]
#[template(path = "login.html")]
struct LoginPage {
    shell: Shell,
    next: String,
    error: Option<String>,
    /// The identity providers offered, each by a button of its own - below
    /// the password form in `users` mode, instead of it on a provider-only
    /// site.
    providers: Vec<ProviderButton>,
    /// Whether a user name and password are taken here (`users` mode).
    password_form: bool,
}

/// One identity provider's button on the sign-in page.
#[derive(Clone)]
pub(crate) struct ProviderButton {
    /// The slug the sign-in link names; empty for the `oidc` mode's own.
    pub id: String,
    pub name: String,
}

#[derive(Template)]
#[template(path = "upload.html")]
struct UploadPage {
    shell: Shell,
    bundles: Vec<UploadBundle>,
    error: Option<String>,
}

/// A bundle an upload may go to.
pub(crate) struct UploadBundle {
    pub id: i64,
    pub name: String,
    /// `content` or `directory`, for the chooser.
    pub kind: String,
    pub file_count: i32,
}

#[derive(Template)]
#[template(path = "edit.html")]
struct EditPage {
    shell: Shell,
    bundle_id: i64,
    bundle_name: String,
    concept_id: String,
    path: String,
    content: String,
    error: Option<String>,
}

#[derive(Template)]
#[template(path = "partials/edit-check.html")]
struct EditCheckPartial {
    error: Option<String>,
    concept_type: Option<String>,
    title: String,
    description: Option<String>,
    body_html: String,
}

#[derive(Template)]
#[template(path = "profile.html")]
struct ProfilePage {
    shell: Shell,
    display: String,
    subject: String,
    role: String,
    actor: String,
    /// How the person was identified, in words.
    how: String,
    permissions: Vec<PermissionView>,
    can_change_password: bool,
    /// Whether sessions can be ended before they expire (a session store
    /// is attached), so the page offers "sign out everywhere".
    sessions_revocable: bool,
    /// How many live sessions the person holds, when that is known.
    session_count: Option<usize>,
    produced: Vec<PersonalItem>,
    verified: Vec<PersonalItem>,
    notice: Option<String>,
    error: Option<String>,
}

/// One rung of the role ladder, marked when the person holds it.
pub(crate) struct PermissionView {
    pub role: String,
    pub text: String,
    pub held: bool,
}

/// Which tab of the Admin page a request is on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AdminTab {
    People,
    Providers,
    Tokens,
    Bundles,
    Settings,
}

impl AdminTab {
    const ALL: [AdminTab; 5] = [
        AdminTab::People,
        AdminTab::Providers,
        AdminTab::Tokens,
        AdminTab::Bundles,
        AdminTab::Settings,
    ];

    const fn href(self) -> &'static str {
        match self {
            AdminTab::People => "/admin/people",
            AdminTab::Providers => "/admin/providers",
            AdminTab::Tokens => "/admin/tokens",
            AdminTab::Bundles => "/admin/bundles",
            AdminTab::Settings => "/admin/settings",
        }
    }

    const fn label(self) -> &'static str {
        match self {
            AdminTab::People => "People",
            AdminTab::Providers => "Identity providers",
            AdminTab::Tokens => "MCP tokens",
            AdminTab::Bundles => "Bundles",
            AdminTab::Settings => "Catalog settings",
        }
    }

    /// What the tab is for, under the heading.
    const fn lede(self) -> &'static str {
        match self {
            AdminTab::People => {
                "Everyone who can sign in here and the role each holds. Every action runs \
                 through the writer role under your name."
            }
            AdminTab::Providers => {
                "The identity providers people may sign in with beside this site's own \
                 passwords: any number of OpenID Connect providers, or GitHub."
            }
            AdminTab::Tokens => {
                "The bearer tokens that let agents reach the MCP endpoint over HTTP."
            }
            AdminTab::Bundles => "The bundles the catalog holds, and what to do with them.",
            AdminTab::Settings => {
                "The catalog's own settings, read from pgokf.get_config(); a database admin \
                 changes them."
            }
        }
    }

    /// The heading of the browser tab.
    fn title(self) -> String {
        format!("Administration · {}", self.label())
    }

    fn shell(self, outcome: AdminOutcome) -> AdminShell {
        AdminShell {
            tabs: Self::ALL
                .iter()
                .map(|tab| AdminTabView {
                    href: tab.href(),
                    label: tab.label(),
                    active: *tab == self,
                })
                .collect(),
            lede: self.lede(),
            notice: outcome.notice,
            error: outcome.error,
        }
    }

    /// Back to this tab, with a notice.
    fn redirect_with(self, notice: &str) -> Response {
        redirect(&format!(
            "{}?notice={}",
            self.href(),
            filters::percent_encode(notice)
        ))
    }
}

/// What every tab of the Admin page shares: the tab strip, what the tab is
/// for, and what the last action had to say.
pub(crate) struct AdminShell {
    pub tabs: Vec<AdminTabView>,
    pub lede: &'static str,
    pub notice: Option<String>,
    pub error: Option<String>,
}

pub(crate) struct AdminTabView {
    pub href: &'static str,
    pub label: &'static str,
    pub active: bool,
}

/// What the Admin page has to say about the action that led to it.
#[derive(Default)]
struct AdminOutcome {
    notice: Option<String>,
    error: Option<String>,
}

#[derive(Template)]
#[template(path = "admin/people.html")]
struct AdminPeoplePage {
    shell: Shell,
    admin: AdminShell,
    /// Whether people are managed here (`users` mode: the catalog's
    /// `pgokf_web.users`).
    users_managed_here: bool,
    /// Whether sessions can be ended here (a session store is attached),
    /// in any mode that issues them.
    sessions_revocable: bool,
    people: Vec<AdminUserView>,
    /// The page's place in the whole: search, page size, page number.
    nav: PeopleNav,
    roles: Vec<String>,
    /// Everyone currently holding a live session, with how many - so an
    /// admin can see whom there is to sign out where no users table lists
    /// people (`oidc` mode).
    live_sessions: Vec<LiveSubject>,
}

/// Where a page of people sits: what was searched, how many to a page,
/// which page, and the links around it - worked out here so the template
/// only prints.
pub(crate) struct PeopleNav {
    pub search: String,
    pub per: usize,
    pub page: usize,
    pub pages: usize,
    pub total: usize,
    /// The rows shown, counted from one; `0..0` when there are none.
    pub first: usize,
    pub last: usize,
    pub prev_href: Option<String>,
    pub next_href: Option<String>,
    /// Every page size offered: the size, its link, whether it is the one.
    pub per_links: Vec<(usize, String, bool)>,
}

/// The page sizes the People tab offers.
const PEOPLE_PER_PAGE: [usize; 4] = [25, 50, 100, 200];
const PEOPLE_PER_PAGE_DEFAULT: usize = 50;

impl PeopleNav {
    fn href(search: &str, per: usize, page: usize) -> String {
        format!(
            "/admin/people?q={}&per={per}&page={page}",
            filters::percent_encode(search)
        )
    }

    fn new(search: String, per: usize, page: usize, total: usize) -> Self {
        let pages = total.div_ceil(per).max(1);
        let page = page.clamp(1, pages);
        // Saturating, because `total` is `usize::MAX` on the first read
        // (before the real count is known) and `page` comes from the query
        // string: the products must not overflow.
        let first = if total == 0 {
            0
        } else {
            (page - 1).saturating_mul(per).saturating_add(1)
        };
        let last = page.saturating_mul(per).min(total);
        Self {
            prev_href: (page > 1).then(|| Self::href(&search, per, page - 1)),
            next_href: (page < pages).then(|| Self::href(&search, per, page + 1)),
            per_links: PEOPLE_PER_PAGE
                .iter()
                .map(|&size| (size, Self::href(&search, size, 1), size == per))
                .collect(),
            search,
            per,
            page,
            pages,
            total,
            first,
            last,
        }
    }

    /// The query, as the store takes it.
    fn query(&self) -> PeopleQuery {
        PeopleQuery {
            search: self.search.clone(),
            offset: (self.page - 1).saturating_mul(self.per),
            limit: self.per,
        }
    }
}

#[derive(Template)]
#[template(path = "admin/providers.html")]
struct AdminProvidersPage {
    shell: Shell,
    admin: AdminShell,
    /// Whether providers can be set up here (`users` mode).
    providers_managed_here: bool,
    providers: Vec<ProviderRow>,
    /// Whether a client secret could be kept (a session secret is set).
    can_seal: bool,
}

/// One identity provider as the list shows it.
pub(crate) struct ProviderRow {
    pub id: String,
    pub name: String,
    pub kind: String,
    pub issuer: String,
    pub enabled: bool,
    pub has_secret: bool,
    pub updated_at: String,
    pub updated_by: String,
    /// Why this instance cannot use it, if so.
    pub trouble: Option<String>,
}

#[derive(Template)]
#[template(path = "admin/provider.html")]
struct AdminProviderPage {
    shell: Shell,
    admin: AdminShell,
    /// The provider's settings as the form shows them (defaults for a new
    /// one).
    provider: ProviderView,
    can_seal: bool,
    /// Why this instance cannot use the stored settings, if so.
    trouble: Option<String>,
    roles: Vec<String>,
}

#[derive(Template)]
#[template(path = "admin/tokens.html")]
struct AdminTokensPage {
    shell: Shell,
    admin: AdminShell,
    /// The MCP bearer tokens (everything but the tokens), newest first.
    mcp_tokens: Vec<McpToken>,
    /// Whether tokens can be minted here (a writer connection is on).
    mcp_tokens_managed_here: bool,
    /// The tenant every token minted here is for, when this UI serves one.
    mcp_tenant: Option<String>,
    mcp_roles: Vec<String>,
    /// A token minted by the request this page answers: shown here, once.
    minted: Option<MintedToken>,
}

#[derive(Template)]
#[template(path = "admin/bundles.html")]
struct AdminBundlesPage {
    shell: Shell,
    admin: AdminShell,
    bundles: Vec<AdminBundle>,
}

#[derive(Template)]
#[template(path = "admin/settings.html")]
struct AdminSettingsPage {
    shell: Shell,
    admin: AdminShell,
    config_json: String,
}

/// An identity provider's settings as the Admin page's form shows them:
/// everything but the client secret, which is never shown.
pub(crate) struct ProviderView {
    /// The slug; empty for a provider not yet added.
    pub id: String,
    pub configured: bool,
    pub enabled: bool,
    /// The kind's id, and every kind the form may choose from.
    pub kind: String,
    pub kinds: Vec<(String, String)>,
    pub issuer: String,
    pub client_id: String,
    pub has_secret: bool,
    pub redirect_url: String,
    pub scopes: String,
    pub subject_claims: String,
    pub groups_claim: String,
    pub provider_name: String,
    pub role_map: String,
    pub default_role: String,
    pub updated_at: String,
    pub updated_by: String,
}

impl ProviderView {
    fn kinds() -> Vec<(String, String)> {
        ProviderKind::all()
            .iter()
            .map(|kind| (kind.id().to_owned(), kind.label().to_owned()))
            .collect()
    }

    /// The form for a provider not yet added.
    fn blank() -> Self {
        Self {
            id: String::new(),
            configured: false,
            enabled: true,
            kind: ProviderKind::Oidc.id().to_owned(),
            kinds: Self::kinds(),
            issuer: String::new(),
            client_id: String::new(),
            has_secret: false,
            redirect_url: String::new(),
            scopes: "openid profile email".to_owned(),
            subject_claims: "sub".to_owned(),
            groups_claim: "groups".to_owned(),
            provider_name: String::new(),
            role_map: String::new(),
            default_role: Role::Viewer.id().to_owned(),
            updated_at: String::new(),
            updated_by: String::new(),
        }
    }

    fn from_settings(s: &ProviderSettings) -> Self {
        Self {
            id: s.id.clone(),
            configured: true,
            enabled: s.enabled,
            kind: s.kind.id().to_owned(),
            kinds: Self::kinds(),
            issuer: s.issuer.clone(),
            client_id: s.client_id.clone(),
            has_secret: s.client_secret.is_some(),
            redirect_url: s.redirect_url.clone(),
            scopes: s.scopes.clone(),
            subject_claims: s.subject_claims.clone(),
            groups_claim: s.groups_claim.clone(),
            provider_name: s.provider_name.clone(),
            role_map: s.role_map.clone(),
            default_role: s.default_role.id().to_owned(),
            updated_at: s.updated_at.clone(),
            updated_by: s.updated_by.clone(),
        }
    }
}

/// A token just minted, for the one page that shows it.
pub(crate) struct MintedToken {
    pub name: String,
    pub role: McpRole,
    pub token: String,
}

pub(crate) struct AdminUserView {
    pub name: String,
    /// What to call them, when that is more than their name.
    pub display: Option<String>,
    pub role: String,
    pub is_me: bool,
    /// The identity provider that brought them (its name, or its slug
    /// when it is gone); `None` for someone who signs in with a password
    /// here, whose password can be reset on this page.
    pub provider: Option<String>,
}

/// One person holding live sessions, for the admin page.
#[derive(Clone)]
pub(crate) struct LiveSubject {
    pub subject: String,
    pub count: usize,
    pub is_me: bool,
}

#[derive(Template)]
#[template(path = "review.html")]
struct ReviewPage {
    shell: Shell,
    items: Vec<ReviewItem>,
    truncated: bool,
    error: Option<String>,
}

#[derive(Template)]
#[template(path = "graph.html")]
struct GraphPage {
    shell: Shell,
    bundles: Vec<BundleInfo>,
    /// Query-string state echoed into the sidebar.
    bundle: String,
    limit_options: Vec<(i32, bool)>,
    /// The JSON endpoint the client draws (the client appends `hops`).
    graph_url: String,
    hops: i32,
    seed_label: Option<String>,
}

#[derive(Template)]
#[template(path = "plugins.html")]
struct PluginsPage {
    shell: Shell,
    form: PluginForm,
    kinds: Vec<KindView>,
    agents: Vec<AgentView>,
    bundles: Vec<BundleInfo>,
    /// The catalog's concept types and its most used tags, for one-click
    /// selectors.
    type_facets: Vec<Facet>,
    tag_facets: Vec<Facet>,
    preview: Option<PluginPreview>,
    error: Option<String>,
}

#[derive(Template)]
#[template(path = "partials/plugin-preview.html")]
struct PluginPreviewPartial {
    form: PluginForm,
    preview: Option<PluginPreview>,
    error: Option<String>,
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

async fn dashboard(State(app): State<Shared>, session: Session) -> PageResult {
    let (version, sql_version) = app.db.versions().await?;
    let health = HealthView::from_json(&app.db.health().await?, version, sql_version);
    let index = IndexView::from_json(&app.db.index_status().await?);
    let stats = app.db.catalog_stats().await?;
    let sync_log = app.db.sync_log(None, 10).await?;
    html(&DashboardPage {
        shell: Shell::new(&app, &session, "Overview", "home"),
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

async fn search_page(
    State(app): State<Shared>,
    session: Session,
    Query(params): Query<SearchParams>,
) -> PageResult {
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
    let mut shell = Shell::new(&app, &session, "Search", "search");
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

async fn bundles_page(State(app): State<Shared>, session: Session) -> PageResult {
    let stats = app.db.catalog_stats().await?;
    html(&BundlesPage {
        shell: Shell::new(&app, &session, "Bundles", "bundles"),
        stats,
    })
}

/// Concepts listed per bundle page; larger bundles page by path.
const BUNDLE_PAGE: usize = 500;

#[derive(Debug, Default, Deserialize)]
struct BundleParams {
    /// Keyset cursor: list concepts whose path sorts after this one.
    #[serde(default)]
    after: String,
    #[serde(default)]
    notice: String,
}

async fn bundle_page(
    State(app): State<Shared>,
    session: Session,
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
    let sync_log = app.db.sync_log(Some(id), 20).await?;
    let bundle_log = app.db.bundle_log(id, 50).await?;
    let title = bundle.name.clone();
    html(&BundlePage {
        shell: Shell::new(&app, &session, &title, "bundles"),
        bundle,
        notice: non_empty(&params.notice),
        concepts,
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
    #[serde(default)]
    notice: String,
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
    session: Session,
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
    let package = match c.concept_type.as_deref() {
        Some("Skill") => app.db.package(bundle_id, &concept_id).await?,
        _ => None,
    };
    let resource = match c.concept_type.as_deref() {
        Some("Script" | "Reference") => app.db.resource(bundle_id, &concept_id).await?,
        _ => None,
    };
    let body_html = match &resource {
        Some(r) if r.textual && !r.is_markdown() => verbatim_body(&c.body_text),
        Some(r) if !r.textual => String::new(),
        // A Markdown reference is stored exactly; render the document, not
        // its search text. The exact bytes come through the audited reader,
        // so rendering a reference leaves the same trail downloading it
        // does - reading `text_body` off the table left none.
        Some(_) => {
            let exact = app
                .db
                .exact_bytes(bundle_id, &concept_id)
                .await?
                .and_then(|exact| String::from_utf8(exact.bytes).ok());
            render_source(&c, &outgoing, exact.as_deref())
        }
        None => render_body(&c, &outgoing),
    };
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
    let workflow = concept_workflow(&app, &session, &c).await?;
    html(&ConceptPage {
        shell: Shell::new(&app, &session, &title, "bundles"),
        notice: non_empty(&params.notice),
        workflow,
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
        package,
        resource,
    })
}

/// A script or plain-text reference shown verbatim in a code block. The
/// fence is longer than any backtick run in the text, so the content cannot
/// close it; the Markdown renderer then escapes it like any code.
fn verbatim_body(text: &str) -> String {
    let longest_run = text.split(|c| c != '`').map(str::len).max().unwrap_or(0);
    let fence = "`".repeat(longest_run.max(2) + 1);
    markdown::render(&format!("{fence}\n{text}\n{fence}\n"))
}

/// The body as HTML: the exact stored Markdown when the catalog keeps
/// source (frontmatter removed, a leading H1 repeating the title dropped
/// since the header shows it, body links resolved through the catalog's
/// own link table), else the search-normalized text.
fn render_body(c: &ConceptDetail, outgoing: &[Link]) -> String {
    let source = c
        .source
        .as_deref()
        .and_then(|bytes| std::str::from_utf8(bytes).ok());
    render_source(c, outgoing, source)
}

/// Render a concept's Markdown `source` (the exact stored document) with the
/// catalog's link resolution, or its search text when there is none.
fn render_source(c: &ConceptDetail, outgoing: &[Link], source: Option<&str>) -> String {
    let title = c.title.as_deref().unwrap_or(&c.concept_id);
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

/// How the client colours a picture: by distance from a seed, or by a
/// group (bundle, or type inside one bundle) with a legend.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ColorBy {
    Hops,
    Bundle,
    Type,
}

/// A graph as the client draws it. Node ids are `bundle_id:concept_id`,
/// unique across bundles; nodes carry their page and graph endpoints so the
/// client never builds URLs from ids; links reference node ids.
fn graph_json(seed: Option<(i64, &str)>, hops: i32, graph: &Graph, color_by: ColorBy) -> Value {
    let node_id = |bundle_id: i64, id: &str| format!("{bundle_id}:{id}");
    let group = |n: &crate::db::GraphNode| match color_by {
        ColorBy::Hops => n.hops.to_string(),
        ColorBy::Bundle => n.bundle_name.clone(),
        ColorBy::Type => n
            .concept_type
            .clone()
            .unwrap_or_else(|| "untyped".to_owned()),
    };
    let mut legend: Vec<String> = Vec::new();
    let nodes: Vec<Value> = graph
        .nodes
        .iter()
        .map(|n| {
            let g = group(n);
            if color_by != ColorBy::Hops && !legend.contains(&g) {
                legend.push(g.clone());
            }
            serde_json::json!({
                "id": node_id(n.bundle_id, &n.id),
                "bundle_id": n.bundle_id,
                "bundle_name": n.bundle_name,
                "concept_id": n.id,
                "title": n.title.as_deref().unwrap_or(&n.id),
                "type": n.concept_type,
                "path": n.path,
                "hops": n.hops,
                "degree": n.degree,
                "group": g,
                "href": concept_href(n.bundle_id, &n.id),
                "graph_href": graph_href(n.bundle_id, &n.id),
            })
        })
        .collect();
    let links: Vec<Value> = graph
        .links
        .iter()
        .map(|l| {
            serde_json::json!({
                "source": node_id(l.bundle_id, &l.source),
                "target": node_id(l.bundle_id, &l.target),
                "count": l.count,
                "relations": l.relations,
                "texts": l.texts,
            })
        })
        .collect();
    legend.sort();
    serde_json::json!({
        "seed": seed.map(|(b, id)| node_id(b, id)),
        "hops": hops,
        "color_by": match color_by {
            ColorBy::Hops => "hops",
            ColorBy::Bundle => "bundle",
            ColorBy::Type => "type",
        },
        "legend": legend,
        "total": graph.total,
        "shown": graph.nodes.len(),
        "nodes": nodes,
        "links": links,
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
    attachment_disposition(&format!("{}.md", file_name_of(concept_id)))
}

/// A package file keeps its own name: resource ids carry their extension,
/// and a manifest is `SKILL.md` (by its class, not its name, so a resource
/// that happens to be called `SKILL` keeps that name).
fn resource_disposition(concept_id: &str, is_manifest: bool) -> String {
    if is_manifest {
        attachment_disposition("SKILL.md")
    } else {
        attachment_disposition(file_name_of(concept_id))
    }
}

/// The last path segment of a concept id, `concept` when it has none.
fn file_name_of(concept_id: &str) -> &str {
    let base = concept_id.rsplit('/').next().unwrap_or("concept");
    if base.is_empty() { "concept" } else { base }
}

/// An attachment header with an ASCII fallback name and the RFC 5987 form.
fn attachment_disposition(name: &str) -> String {
    let ascii: String = name
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
        "attachment; filename=\"{ascii}\"; filename*=UTF-8''{}",
        filters::percent_encode(name)
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

async fn status_page(State(app): State<Shared>, session: Session) -> PageResult {
    let (version, sql_version) = app.db.versions().await?;
    let health = HealthView::from_json(&app.db.health().await?, version, sql_version);
    let index = IndexView::from_json(&app.db.index_status().await?);
    let config_json = serde_json::to_string_pretty(&app.db.config().await?).unwrap_or_default();
    let stale = app.db.stale().await?;
    let duplicates = app.db.duplicates().await?;
    let sync_log = app.db.sync_log(None, 50).await?;
    html(&StatusPage {
        shell: Shell::new(&app, &session, "Operations", "status"),
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
// Human workflow: sign in, upload, edit, review
// ---------------------------------------------------------------------------

/// The writer connection and the person, once a workflow page has checked
/// both.
struct Access<'a> {
    writer: &'a Db,
    who: Principal,
}

/// Gate a workflow page: the server must have a writer connection, and the
/// person must hold `role`. Nobody signed in is sent to the login page
/// when there is one, and refused otherwise.
fn require<'a>(
    app: &'a App,
    session: &Session,
    role: Role,
    next: &str,
) -> Result<Access<'a>, AppError> {
    let writer = app.writer.as_ref().ok_or_else(|| {
        AppError::unavailable(
            "The human workflow is off: this server has no writer connection (OKF_PG_WRITER_URL).",
        )
    })?;
    let who = match &session.principal {
        Some(person) => person.clone(),
        None if session.mode == Mode::Users => return Err(AppError::sign_in(next)),
        None => return Err(AppError::forbidden(role)),
    };
    if !who.role.allows(role) {
        return Err(AppError::forbidden(role));
    }
    Ok(Access { writer, who })
}

/// The workflow rebuilds a content bundle from the sources the catalog
/// keeps, so it needs `store_source` on.
async fn ensure_sources(db: &Db) -> Result<(), AppError> {
    if db.keeps_sources().await? {
        Ok(())
    } else {
        Err(AppError::unavailable(
            "The catalog does not keep document sources (its store_source setting is off), so a \
             document cannot be rebuilt after a change. An admin turns it on with \
             pgokf.set_config('store_source', 'true') and refreshes the bundles.",
        ))
    }
}

/// Apply a change to a bundle through its store, one change at a time
/// across the process (a content resync is a full snapshot; a directory
/// write plus refresh must not interleave with another).
async fn apply_change(
    app: &App,
    writer: &Db,
    store: &DocumentStore,
    changes: Vec<BundleFile>,
    removals: &[String],
) -> Result<SyncOutcome, AppError> {
    let _one_at_a_time = app.rebuilds.lock().await;
    // A content change rewrites the whole bundle, so it must not interleave
    // with another writer's - and the other writer may be pgokf-mcp or a
    // second instance of this UI, which this process's lock says nothing
    // about. Held until the change is done, released with the connection.
    let _across_writers = match store.content_name() {
        Some(name) => Some(writer.lock_content_bundle(name).await?),
        None => None,
    };
    Ok(store.apply(writer, changes, removals).await?)
}

/// The store of a bundle, or why it cannot be changed from here.
async fn open_store(app: &App, bundle_id: i64) -> Result<Result<DocumentStore, String>, AppError> {
    let Some((kind, path)) = app.db.bundle_source(bundle_id).await? else {
        return Ok(Err("This bundle is not available.".to_owned()));
    };
    Ok(match kind.as_str() {
        "content" => app
            .db
            .content_bundle(bundle_id)
            .await?
            .map(DocumentStore::Content)
            .ok_or_else(|| "This content bundle is not available.".to_owned()),
        "filesystem" => match app.stores.local_dir(&path) {
            Some(root) if root.is_dir() => Ok(DocumentStore::Directory { bundle_id, root }),
            Some(root) => Err(format!(
                "This bundle's directory is not reachable here ({} is missing); change the \
                 document at its source.",
                root.display()
            )),
            None => Err(
                "This bundle is synced from a directory the UI cannot reach: set \
                 OKF_WEB_BUNDLES_DIR (mounted read-write) to edit it here, or change the \
                 document at its source."
                    .to_owned(),
            ),
        },
        _ => Err(
            "This bundle is synced from an object store; change the document at its source."
                .to_owned(),
        ),
    })
}

/// Whether the workflow can rebuild through this store: a content bundle
/// needs the catalog to keep sources.
async fn ensure_store_sources(store: &DocumentStore, writer: &Db) -> Result<(), AppError> {
    if !store.rebuilds_from_catalog() {
        return Ok(());
    }
    ensure_sources(writer).await?;
    // Rebuilding sends back what the catalog stores. A bundle carrying
    // anything it does not store the bytes of would come back without it,
    // so the change is refused rather than made at that cost.
    if let Some(carries) = writer.bundle_carries_unstored(store.bundle_id()).await? {
        return Err(AppError::unavailable(format!(
            "This bundle carries {carries}, whose bytes the catalog does not keep. Changing one \
             document rewrites the whole bundle from what it does keep, which would drop them, \
             so the change is refused; change this bundle where its files come from."
        )));
    }
    Ok(())
}

/// A content bundle name as `register_bundle_content` keys it.
fn validated_bundle_name(name: &str) -> Result<String, AppError> {
    let name = name.trim();
    let plain = |c: char| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-');
    if name.is_empty() || name.len() > 64 || !name.chars().all(plain) || name.starts_with('.') {
        return Err(AppError::bad_request(
            "A bundle name is 1 to 64 letters, digits, dots, underscores, or hyphens, not \
             starting with a dot.",
        ));
    }
    Ok(name.to_owned())
}

/// A directory inside the bundle: plain segments, no climbing.
fn validated_directory(dir: &str) -> Result<String, AppError> {
    let dir = dir.trim().trim_matches('/');
    let plain = |c: char| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-');
    let ok = dir.is_empty()
        || dir.split('/').all(|seg| {
            !seg.is_empty() && seg != "." && seg != ".." && seg != ".git" && seg.chars().all(plain)
        });
    if ok && dir.len() <= 200 {
        Ok(dir.to_owned())
    } else {
        Err(AppError::bad_request(
            "The directory must be plain path segments (letters, digits, dots, underscores, \
             hyphens) without . or ..",
        ))
    }
}

/// Whether a path names a Markdown file.
fn is_markdown(path: &str) -> bool {
    std::path::Path::new(path)
        .extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case("md"))
}

/// The file name of an uploaded document: its base name, a Markdown file.
fn validated_file_name(name: &str) -> Result<String, AppError> {
    let base = name.rsplit(['/', '\\']).next().unwrap_or("").trim();
    let plain = |c: char| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | ' ');
    let ok = base.len() > 3
        && base.len() <= 200
        && is_markdown(base)
        && !base.starts_with('.')
        && base.chars().all(plain);
    if ok {
        Ok(base.replace(' ', "-"))
    } else {
        Err(AppError::bad_request(format!(
            "{name:?} is not a Markdown document name (letters, digits, dots, underscores, \
             hyphens, ending in .md)"
        )))
    }
}

/// What the signed-in person may do to this concept.
async fn concept_workflow(
    app: &App,
    session: &Session,
    c: &ConceptDetail,
) -> Result<ConceptWorkflow, AppError> {
    if app.writer.is_none() || !session.allows(Role::Editor) && !session.allows(Role::Approver) {
        return Ok(ConceptWorkflow::default());
    }
    let blocker = match open_store(app, c.bundle_id).await? {
        Err(reason) => Some(reason),
        Ok(_) if !is_markdown(&c.path) => Some(
            "Only Markdown documents are edited here; package files come with their skill."
                .to_owned(),
        ),
        Ok(DocumentStore::Content(_)) if c.source.is_none() => Some(
            "The catalog holds no source for this document (store_source was off when it was \
             ingested)."
                .to_owned(),
        ),
        Ok(_) => None,
    };
    let editable = blocker.is_none();
    Ok(ConceptWorkflow {
        can_edit: editable && session.allows(Role::Editor),
        can_review: editable && session.allows(Role::Approver),
        blocker: blocker.filter(|_| session.allows(Role::Editor)),
    })
}

/// A document with its current text, for editing or reviewing: from the
/// bundle's directory when the UI can reach it, else from the source the
/// catalog stored.
async fn editable_document(
    app: &App,
    bundle_id: i64,
    concept_id: &str,
) -> Result<(DocumentStore, ConceptDetail, String), AppError> {
    let store = open_store(app, bundle_id)
        .await?
        .map_err(AppError::bad_request)?;
    let concept = app
        .db
        .concept(bundle_id, concept_id)
        .await?
        .ok_or_else(|| AppError::not_found("This concept"))?;
    if !is_markdown(&concept.path) {
        return Err(AppError::bad_request(
            "Only Markdown documents are edited here; package files come with their skill.",
        ));
    }
    let source = match store.read(&concept.path).map_err(AppError::from)? {
        Some(bytes) => bytes,
        None => concept.source.clone().ok_or_else(|| {
            AppError::bad_request(
                "The catalog holds no source for this document (store_source was off when it \
                 was ingested).",
            )
        })?,
    };
    let text = String::from_utf8(source)
        .map_err(|_| AppError::bad_request("The stored source is not UTF-8 text."))?;
    Ok((store, concept, text))
}

#[derive(Debug, Default, Deserialize)]
struct NextParams {
    #[serde(default)]
    next: String,
}

/// The buttons for `providers`, in their order.
fn provider_buttons(providers: &[Arc<OidcAuth>]) -> Vec<ProviderButton> {
    providers
        .iter()
        .map(|provider| ProviderButton {
            id: provider.stored_id().unwrap_or_default().to_owned(),
            name: provider.provider_name().to_owned(),
        })
        .collect()
}

/// The sign-in page: a user name and password in `users` mode, with a
/// button to every identity provider an admin has set up below them; on a
/// provider-only site there is nothing to type, and the person goes
/// straight to the provider.
async fn login_page(
    State(app): State<Shared>,
    session: Session,
    Query(params): Query<NextParams>,
) -> PageResult {
    if !session.mode.is_local_session() {
        return Err(AppError::not_found("This page"));
    }
    let next = safe_next(&params.next);
    if session.principal.is_some() {
        return Ok(redirect(&next));
    }
    let providers = app.auth.providers().await?;
    if matches!(session.mode, Mode::Oidc) {
        return match providers.first() {
            Some(provider) => start_provider(&app, &session, provider, &next).await,
            None => Err(AppError::not_found("This page")),
        };
    }
    html(&LoginPage {
        shell: Shell::new(&app, &session, "Sign in", "login"),
        next,
        error: None,
        providers: provider_buttons(&providers),
        password_form: true,
    })
}

#[derive(Debug, Default, Deserialize)]
struct ProviderParams {
    /// The provider's slug; empty for the `oidc` mode's own.
    #[serde(default)]
    with: String,
    #[serde(default)]
    next: String,
}

/// An identity provider's sign-in, from its button below the password
/// form - or the only way in, on a provider-only site.
async fn login_provider(
    State(app): State<Shared>,
    session: Session,
    Query(params): Query<ProviderParams>,
) -> PageResult {
    if !session.mode.is_local_session() {
        return Err(AppError::not_found("This page"));
    }
    let next = safe_next(&params.next);
    if session.principal.is_some() {
        return Ok(redirect(&next));
    }
    let wanted = non_empty(&params.with);
    let Some(provider) = app.auth.provider(wanted.as_deref()).await? else {
        return Err(AppError::not_found("This page"));
    };
    start_provider(&app, &session, &provider, &next).await
}

/// Send the person to the provider, with the cookie that remembers this
/// attempt; when the provider cannot be reached, say so on the sign-in
/// page instead.
async fn start_provider(
    app: &App,
    session: &Session,
    provider: &OidcAuth,
    next: &str,
) -> PageResult {
    match provider.start(next).await {
        Ok((url, cookie)) => {
            let mut response = redirect(&url);
            if let Some(value) = cookie_header(&cookie) {
                response.headers_mut().append(header::SET_COOKIE, value);
            }
            Ok(response)
        }
        Err(error) => {
            eprintln!(
                "pgokf-web: the sign-in through {} could not be started: {error:#}",
                provider.provider_name()
            );
            let providers = app.auth.providers().await.unwrap_or_default();
            let mut response = html(&LoginPage {
                shell: Shell::new(app, session, "Sign in", "login"),
                next: next.to_owned(),
                error: Some(format!(
                    "{} could not be reached just now.",
                    provider.provider_name()
                )),
                providers: provider_buttons(&providers),
                password_form: matches!(session.mode, Mode::Users),
            })?;
            *response.status_mut() = StatusCode::BAD_GATEWAY;
            Ok(response)
        }
    }
}

/// What the provider sends back to the redirect URL: a code, or a refusal.
#[derive(Debug, Default, Deserialize)]
struct CallbackParams {
    #[serde(default)]
    code: String,
    #[serde(default)]
    state: String,
    #[serde(default)]
    error: String,
}

/// What the provider's answer says before any code is exchanged: what to
/// tell the person, and with which status, when it is a refusal.
fn callback_refusal(params: &CallbackParams, provider_name: &str) -> Option<(String, StatusCode)> {
    if !params.error.trim().is_empty() {
        // The provider's error code is a fixed token; nothing else it sent
        // is repeated back to the browser.
        let code: String = params
            .error
            .chars()
            .filter(|c| c.is_ascii_alphanumeric() || *c == '_')
            .take(64)
            .collect();
        eprintln!("pgokf-web: {provider_name} refused a sign-in ({code})");
        return Some((
            format!("{provider_name} did not sign you in ({code})."),
            StatusCode::UNAUTHORIZED,
        ));
    }
    if params.code.trim().is_empty() {
        return Some((
            "That sign-in carried no code.".to_owned(),
            StatusCode::BAD_REQUEST,
        ));
    }
    None
}

/// The person's row on the People table, in the `users` mode, so an admin
/// sees them and can set their role: `Some(why)` when the name is somebody
/// else's and the sign-in must be refused.
async fn admit_to_people(
    app: &App,
    person: &Principal,
    oidc: &OidcAuth,
) -> Result<Option<String>, AppError> {
    let Some(users) = app.auth.users() else {
        return Ok(None);
    };
    match users.admit(person, oidc).await? {
        Admission::New => {
            eprintln!(
                "pgokf-web: {} ({}) is a new person here, through {}",
                person.display,
                person.actor(),
                oidc.provider_name()
            );
            Ok(None)
        }
        Admission::Known => Ok(None),
        Admission::Refused(why) => {
            eprintln!(
                "pgokf-web: refused a sign-in through {} as {} ({}): {why}",
                oidc.provider_name(),
                person.display,
                person.actor()
            );
            Ok(Some(why))
        }
    }
}

/// The provider's answer: find which provider this sign-in started at,
/// check the answer, open a session, and go on to the page the person was
/// heading for.
async fn auth_callback(
    State(app): State<Shared>,
    session: Session,
    headers: HeaderMap,
    Query(params): Query<CallbackParams>,
) -> PageResult {
    let sessions = app
        .auth
        .sessions()
        .ok_or_else(|| AppError::not_found("This page"))?;
    // Whatever happens, this attempt is over: the cookie goes.
    let drop_flow = sessions.clear_flow();
    let buttons = provider_buttons(&app.auth.providers().await?);
    let refused = |message: String, status: StatusCode| -> PageResult {
        let mut response = html(&LoginPage {
            shell: Shell::new(&app, &session, "Sign in", "login"),
            next: "/".to_owned(),
            error: Some(message),
            providers: buttons.clone(),
            password_form: matches!(session.mode, Mode::Users),
        })?;
        *response.status_mut() = status;
        if let Some(value) = cookie_header(&drop_flow) {
            response.headers_mut().append(header::SET_COOKIE, value);
        }
        Ok(response)
    };
    // The flow cookie names the provider this sign-in started at; only
    // that one, as set up now, may finish it.
    let Some(started_at) = crate::oidc::flow_provider(sessions, &headers) else {
        return refused(
            "This sign-in did not start here, or it took too long. Start again.".to_owned(),
            StatusCode::UNAUTHORIZED,
        );
    };
    let Some(oidc) = app.auth.provider(started_at.id()).await? else {
        return refused(
            "That identity provider is no longer offered here.".to_owned(),
            StatusCode::UNAUTHORIZED,
        );
    };
    if let Some((message, status)) = callback_refusal(&params, oidc.provider_name()) {
        return refused(message, status);
    }
    match oidc.complete(&headers, &params.code, &params.state).await {
        Ok((person, groups, next)) => {
            if let Some(why) = admit_to_people(&app, &person, &oidc).await? {
                return refused(why, StatusCode::FORBIDDEN);
            }
            let cookie = sessions
                .open_session_with(
                    &person.subject,
                    Mode::Oidc,
                    oidc.binding(),
                    Some(person.display.clone()),
                    groups,
                    oidc.stored_id(),
                )
                .await?;
            eprintln!(
                "pgokf-web: {} ({}) signed in through {}",
                person.display,
                person.actor(),
                oidc.provider_name()
            );
            let mut response = redirect(&safe_next(&next));
            for value in [&drop_flow, &cookie]
                .into_iter()
                .filter_map(|c| cookie_header(c))
            {
                response.headers_mut().append(header::SET_COOKIE, value);
            }
            Ok(response)
        }
        Err(error) => {
            eprintln!(
                "pgokf-web: a sign-in through {} did not complete: {error:#}",
                oidc.provider_name()
            );
            refused(
                "That sign-in could not be completed. Start again.".to_owned(),
                StatusCode::UNAUTHORIZED,
            )
        }
    }
}

// No `Debug`: the form carries a password.
#[derive(Deserialize)]
struct LoginForm {
    username: String,
    password: String,
    #[serde(default)]
    next: String,
}

async fn login_submit(
    State(app): State<Shared>,
    session: Session,
    headers: axum::http::HeaderMap,
    Form(form): Form<LoginForm>,
) -> PageResult {
    let users = app
        .auth
        .users()
        .ok_or_else(|| AppError::not_found("This page"))?;
    // The address the throttle keys on: behind a trusted proxy that is the
    // real client, not the one proxy every request arrives from, so one
    // attacker cannot hold an account under cooldown for everyone.
    let client = app.trusted_proxies.client_ip(&headers, session.peer);
    // Held across the verification only: Argon2id is expensive by design,
    // and this page is open to anyone.
    let permit = users.permit().await;
    let verified = users.verify(&form.username, &form.password, client).await;
    drop(permit);
    // A store that could not answer is a 503 here, not a failed sign-in.
    let verified = verified?;
    if let Some(person) = verified {
        let cookie = users.issue_cookie(&person).await?;
        let mut response = redirect(&safe_next(&form.next));
        if let Some(value) = cookie_header(&cookie) {
            response.headers_mut().insert(header::SET_COOKIE, value);
        }
        return Ok(response);
    }
    // A pause between attempts, so guessing costs time; after a few
    // failures the name waits out a cooldown.
    tokio::time::sleep(Duration::from_millis(400)).await;
    let error = match users.cooldown(&form.username, client) {
        Some(wait) => format!(
            "Too many failed attempts for this name; try again in {} second{}.",
            wait.as_secs().max(1),
            if wait.as_secs().max(1) == 1 { "" } else { "s" }
        ),
        None => "Unknown user name or wrong password.".to_owned(),
    };
    let mut response = html(&LoginPage {
        shell: Shell::new(&app, &session, "Sign in", "login"),
        next: safe_next(&form.next),
        error: Some(error),
        providers: provider_buttons(&app.auth.providers().await?),
        password_form: true,
    })?;
    *response.status_mut() = StatusCode::UNAUTHORIZED;
    Ok(response)
}

/// End this site's session, and the provider's too when it offers to.
async fn logout(State(app): State<Shared>, headers: axum::http::HeaderMap) -> PageResult {
    let Some(sessions) = app.auth.sessions() else {
        return Ok(redirect("/"));
    };
    // End the session itself, not merely this browser's copy of the cookie:
    // a copy taken elsewhere stops working now too. If that fails, say so -
    // this browser's cookie is still cleared, but a copy would keep working,
    // and a sign-out that quietly did not happen is worse than an error.
    let mode = sessions
        .presented_mode(&headers)
        .unwrap_or_else(|| app.auth.mode());
    let ended = sessions.end_session_from(&headers, mode).await;
    let mut response = match ended {
        Ok(()) => {
            // The provider's own sign-out, when it was the provider that
            // signed the person in and it offers one.
            let onward = match mode {
                Mode::Oidc => {
                    let opened_by = sessions
                        .presented_claims(&headers)
                        .and_then(|claims| claims.provider);
                    app.auth
                        .provider(opened_by.as_deref())
                        .await
                        .ok()
                        .flatten()
                        .and_then(|provider| provider.end_session_url())
                }
                _ => None,
            };
            redirect(&onward.unwrap_or_else(|| "/".to_owned()))
        }
        Err(error) => {
            eprintln!("pgokf-web: ending a session on sign-out: {error:#}");
            AppError::with(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Your session could not be ended on the server, so a copy of it elsewhere \
                 would still work. This browser is signed out; try again, or ask an admin \
                 to sign you out everywhere.",
            )
            .into_response()
        }
    };
    for cookie in [sessions.clear_session(), sessions.clear_flow()] {
        if let Some(value) = cookie_header(&cookie) {
            response.headers_mut().append(header::SET_COOKIE, value);
        }
    }
    Ok(response)
}

async fn upload_page(State(app): State<Shared>, session: Session) -> PageResult {
    require(&app, &session, Role::Uploader, "/upload")?;
    html(&UploadPage {
        shell: Shell::new(&app, &session, "Upload documents", "upload"),
        bundles: upload_bundles(&app).await?,
        error: None,
    })
}

/// Where an upload goes.
enum UploadTarget {
    Existing(DocumentStore),
    New(String),
}

/// The bundles an upload may go to: content bundles, and directory bundles
/// this process can write.
async fn upload_bundles(app: &App) -> Result<Vec<UploadBundle>, AppError> {
    let mut targets: Vec<UploadBundle> = app
        .db
        .content_bundles()
        .await?
        .into_iter()
        .map(|b| UploadBundle {
            id: b.id,
            name: b.name,
            kind: "content".to_owned(),
            file_count: b.file_count,
        })
        .collect();
    if app.stores.local_root.is_some() {
        for bundle in app.db.bundles().await? {
            if let Ok(DocumentStore::Directory { .. }) = open_store(app, bundle.id).await? {
                targets.push(UploadBundle {
                    id: bundle.id,
                    name: bundle.name,
                    kind: "directory".to_owned(),
                    file_count: bundle.file_count,
                });
            }
        }
    }
    Ok(targets)
}

/// The fields of the upload form, read from the multipart body.
struct UploadFields {
    bundle: String,
    new_bundle: String,
    directory: String,
    files: Vec<(String, Vec<u8>)>,
}

async fn read_upload(mut multipart: Multipart) -> Result<UploadFields, AppError> {
    let mut fields = UploadFields {
        bundle: String::new(),
        new_bundle: String::new(),
        directory: String::new(),
        files: Vec::new(),
    };
    let bad = |e: axum::extract::multipart::MultipartError| {
        AppError::bad_request(format!("The upload could not be read: {e}"))
    };
    while let Some(field) = multipart.next_field().await.map_err(bad)? {
        match field.name().unwrap_or("") {
            "bundle" => fields.bundle = field.text().await.map_err(bad)?,
            "new_bundle" => fields.new_bundle = field.text().await.map_err(bad)?,
            "directory" => fields.directory = field.text().await.map_err(bad)?,
            "files" => {
                let name = field.file_name().unwrap_or("").to_owned();
                let bytes = field.bytes().await.map_err(bad)?;
                if !name.is_empty() && !bytes.is_empty() {
                    fields.files.push((name, bytes.to_vec()));
                }
                if fields.files.len() > MAX_UPLOAD_FILES {
                    return Err(AppError::bad_request(format!(
                        "At most {MAX_UPLOAD_FILES} documents per upload."
                    )));
                }
            }
            _ => {}
        }
    }
    Ok(fields)
}

async fn upload_submit(
    State(app): State<Shared>,
    session: Session,
    multipart: Multipart,
) -> PageResult {
    let access = require(&app, &session, Role::Uploader, "/upload")?;
    match upload_documents(&app, &access, multipart).await {
        Ok(response) => Ok(response),
        // The form comes back with the problem instead of an error page.
        Err(error) if error.status() == StatusCode::BAD_REQUEST => {
            let mut response = html(&UploadPage {
                shell: Shell::new(&app, &session, "Upload documents", "upload"),
                bundles: upload_bundles(&app).await?,
                error: Some(error.message().to_owned()),
            })?;
            *response.status_mut() = StatusCode::BAD_REQUEST;
            Ok(response)
        }
        Err(error) => Err(error),
    }
}

async fn upload_documents(app: &App, access: &Access<'_>, multipart: Multipart) -> PageResult {
    let fields = read_upload(multipart).await?;
    if fields.files.is_empty() {
        return Err(AppError::bad_request(
            "Choose at least one Markdown document.",
        ));
    }
    let target = if let Some(id) = non_empty(&fields.bundle) {
        let id: i64 = id
            .parse()
            .map_err(|_| AppError::bad_request("Choose a bundle."))?;
        let store = open_store(app, id).await?.map_err(AppError::bad_request)?;
        ensure_store_sources(&store, access.writer).await?;
        UploadTarget::Existing(store)
    } else {
        ensure_sources(access.writer).await?;
        let name = validated_bundle_name(&fields.new_bundle)?;
        // `register_bundle_content` is a full snapshot resync, so calling it
        // for an existing name with only the files in hand would delete
        // every other document in that bundle. Choosing it in the list goes
        // through `apply_change`, which merges.
        if app.db.content_bundle_exists(&name).await? {
            return Err(AppError::bad_request(format!(
                "A bundle called {name} already exists. Choose it in the list to add to it; \
                 creating it again would replace everything already in it."
            )));
        }
        UploadTarget::New(name)
    };
    let directory = validated_directory(&fields.directory)?;
    let now = now_iso();
    let actor = access.who.actor();
    let mut documents = Vec::with_capacity(fields.files.len());
    for (name, bytes) in &fields.files {
        let file_name = validated_file_name(name)?;
        let path = if directory.is_empty() {
            file_name
        } else {
            format!("{directory}/{file_name}")
        };
        let text = std::str::from_utf8(bytes)
            .map_err(|_| AppError::bad_request(format!("{name} is not UTF-8 text.")))?;
        let mut document =
            Document::parse(text).map_err(|e| AppError::bad_request(format!("{name}: {e}")))?;
        // A verification is granted by an approver here, never uploaded, and
        // an upload may not put another person's name to its origin.
        document
            .contribute_new(&actor, &now)
            .map_err(|why| AppError::bad_request(format!("{name}: {why}")))?;
        document
            .validate(&path)
            .map_err(|e| AppError::bad_request(format!("{name}: {e}")))?;
        documents.push(BundleFile {
            path,
            bytes: document.render().into_bytes(),
        });
    }
    // Adding is an uploader's; replacing is an editor's. Without this an
    // uploader could overwrite any document by uploading at its path - and
    // the upload sets earlier verifications aside, so it also knocked an
    // approved document back into the review queue.
    if !access.who.role.allows(Role::Editor)
        && let UploadTarget::Existing(store) = &target
    {
        for document in &documents {
            if store.holds(access.writer, &document.path).await? {
                return Err(AppError::bad_request(format!(
                    "{} already exists in this bundle. Replacing a document needs the editor \
                     role; upload it under a different name, or ask an editor.",
                    document.path
                )));
            }
        }
    }
    let count = documents.len();
    let outcome = match &target {
        UploadTarget::Existing(store) => {
            apply_change(app, access.writer, store, documents, &[]).await?
        }
        UploadTarget::New(name) => {
            let _one_at_a_time = app.rebuilds.lock().await;
            access.writer.register_content(name, &documents).await?
        }
    };
    eprintln!(
        "pgokf-web: {} uploaded {count} document(s) into bundle {} ({} added, {} updated)",
        access.who.actor(),
        outcome.bundle_id,
        outcome.added,
        outcome.updated
    );
    Ok(redirect(&format!(
        "/bundles/{}?notice={}",
        outcome.bundle_id,
        filters::percent_encode(&format!(
            "Uploaded {count} document{}: {} added, {} updated.",
            if count == 1 { "" } else { "s" },
            outcome.added,
            outcome.updated
        ))
    )))
}

async fn edit_page(
    State(app): State<Shared>,
    session: Session,
    Path((bundle_id, concept_id)): Path<(i64, String)>,
) -> PageResult {
    let next = format!("/edit/{bundle_id}/{concept_id}");
    let access = require(&app, &session, Role::Editor, &next)?;
    let (store, concept, content) = editable_document(&app, bundle_id, &concept_id).await?;
    ensure_store_sources(&store, access.writer).await?;
    html(&EditPage {
        shell: Shell::new(&app, &session, &format!("Edit {}", concept.path), "bundles"),
        bundle_id,
        bundle_name: concept.bundle_name.clone(),
        concept_id,
        path: concept.path,
        content,
        error: None,
    })
}

#[derive(Debug, Deserialize)]
struct EditForm {
    #[serde(default)]
    content: String,
    #[serde(default)]
    action: String,
}

async fn edit_submit(
    State(app): State<Shared>,
    session: Session,
    Path((bundle_id, concept_id)): Path<(i64, String)>,
    Form(form): Form<EditForm>,
) -> PageResult {
    let next = format!("/edit/{bundle_id}/{concept_id}");
    let access = require(&app, &session, Role::Editor, &next)?;
    let (store, concept, current) = editable_document(&app, bundle_id, &concept_id).await?;
    ensure_store_sources(&store, access.writer).await?;
    let concept_url = concept_href(bundle_id, &concept_id);
    if form.action == "delete" {
        apply_change(
            &app,
            access.writer,
            &store,
            Vec::new(),
            std::slice::from_ref(&concept.path),
        )
        .await?;
        eprintln!(
            "pgokf-web: {} deleted {}:{}",
            access.who.actor(),
            bundle_id,
            concept.path
        );
        return Ok(redirect(&format!(
            "/bundles/{bundle_id}?notice={}",
            filters::percent_encode(&format!("Deleted {}.", concept.path))
        )));
    }
    let submitted = form.content.replace("\r\n", "\n");
    if submitted == current {
        return Ok(redirect(&format!(
            "{concept_url}?notice={}",
            filters::percent_encode("No changes to save.")
        )));
    }
    let stored = Document::parse(&current).ok();
    let checked = Document::parse(&submitted).and_then(|mut document| {
        // The stored verifications are carried over and set aside with any
        // the editor typed: an edit always goes back to review.
        document.contribute_edit(&access.who.actor(), &now_iso(), stored.as_ref());
        document.validate(&concept.path).map(|()| document)
    });
    let document = match checked {
        Ok(document) => document,
        Err(problem) => {
            let mut response = html(&EditPage {
                shell: Shell::new(&app, &session, &format!("Edit {}", concept.path), "bundles"),
                bundle_id,
                bundle_name: concept.bundle_name.clone(),
                concept_id,
                path: concept.path,
                content: submitted,
                error: Some(problem),
            })?;
            *response.status_mut() = StatusCode::BAD_REQUEST;
            return Ok(response);
        }
    };
    let had_verification = stored
        .as_ref()
        .is_some_and(Document::has_human_verification);
    apply_change(
        &app,
        access.writer,
        &store,
        vec![BundleFile {
            path: concept.path.clone(),
            bytes: document.render().into_bytes(),
        }],
        &[],
    )
    .await?;
    eprintln!(
        "pgokf-web: {} edited {}:{}",
        access.who.actor(),
        bundle_id,
        concept.path
    );
    let notice = if had_verification {
        "Saved. The earlier verification was set aside, so the document is back in the review queue."
    } else {
        "Saved."
    };
    Ok(redirect(&format!(
        "{concept_url}?notice={}",
        filters::percent_encode(notice)
    )))
}

async fn edit_check(
    State(app): State<Shared>,
    session: Session,
    Path((bundle_id, concept_id)): Path<(i64, String)>,
    Form(form): Form<EditForm>,
) -> PageResult {
    require(&app, &session, Role::Editor, "/")?;
    let path = format!("{concept_id}.md");
    let _ = bundle_id;
    let checked = Document::parse(&form.content.replace("\r\n", "\n"))
        .and_then(|document| document.validate(&path).map(|()| document));
    let partial = match checked {
        Ok(document) => EditCheckPartial {
            error: None,
            concept_type: document.text("type").map(str::to_owned),
            title: document.text("title").unwrap_or("(untitled)").to_owned(),
            description: document.text("description").map(str::to_owned),
            body_html: markdown::render(&document.body),
        },
        Err(problem) => EditCheckPartial {
            error: Some(problem),
            concept_type: None,
            title: String::new(),
            description: None,
            body_html: String::new(),
        },
    };
    html(&partial)
}

async fn review_page(State(app): State<Shared>, session: Session) -> PageResult {
    let access = require(&app, &session, Role::Approver, "/review")?;
    let error = ensure_sources(access.writer)
        .await
        .err()
        .map(|e| e.message().to_owned());
    let mut items = app.db.review_queue(REVIEW_LIMIT + 1).await?;
    let truncated = i64::try_from(items.len()).unwrap_or(i64::MAX) > REVIEW_LIMIT;
    items.truncate(usize::try_from(REVIEW_LIMIT).unwrap_or(usize::MAX));
    html(&ReviewPage {
        shell: Shell::new(&app, &session, "Review queue", "review"),
        items,
        truncated,
        error,
    })
}

#[derive(Debug, Deserialize)]
struct ReviewForm {
    #[serde(default)]
    decision: String,
    #[serde(default)]
    note: String,
}

async fn concept_review(
    State(app): State<Shared>,
    session: Session,
    Path((bundle_id, concept_id)): Path<(i64, String)>,
    Form(form): Form<ReviewForm>,
) -> PageResult {
    let concept_url = concept_href(bundle_id, &concept_id);
    let access = require(&app, &session, Role::Approver, &concept_url)?;
    let (store, concept, current) = editable_document(&app, bundle_id, &concept_id).await?;
    ensure_store_sources(&store, access.writer).await?;
    let mut document = Document::parse(&current)
        .map_err(|e| AppError::bad_request(format!("The stored document does not parse: {e}")))?;
    let actor = access.who.actor();
    let note = non_empty(&form.note);
    let outcome = match form.decision.as_str() {
        "approve" => {
            document.record_verification(&actor, &now_iso(), note.as_deref());
            "Approved: the document is now human-reviewed."
        }
        "send-back" => {
            document.send_back(&actor, &now_iso(), note.as_deref());
            "Sent back: the document is a draft again."
        }
        _ => return Err(AppError::bad_request("Choose approve or send back.")),
    };
    document.validate(&concept.path).map_err(|e| {
        AppError::bad_request(format!("The reviewed document would not parse: {e}"))
    })?;
    apply_change(
        &app,
        access.writer,
        &store,
        vec![BundleFile {
            path: concept.path.clone(),
            bytes: document.render().into_bytes(),
        }],
        &[],
    )
    .await?;
    eprintln!(
        "pgokf-web: {actor} reviewed {bundle_id}:{} ({})",
        concept.path, form.decision
    );
    Ok(redirect(&format!(
        "{concept_url}?notice={}",
        filters::percent_encode(outcome)
    )))
}

// ---------------------------------------------------------------------------
// Profile and administration
// ---------------------------------------------------------------------------

/// The signed-in person, or the way to become one.
fn signed_in(session: &Session, next: &str) -> Result<Principal, AppError> {
    match (&session.principal, session.mode) {
        (Some(person), _) => Ok(person.clone()),
        (None, mode) if mode.is_local_session() => Err(AppError::sign_in(next)),
        (None, Mode::Header) => Err(AppError::forbidden_message(
            "Sign in through the site's identity provider.",
        )),
        (None, _) => Err(AppError::not_found("This page")),
    }
}

/// What each rung of the ladder allows, in words.
fn permission_views(role: Role) -> Vec<PermissionView> {
    Role::all()
        .iter()
        .map(|r| PermissionView {
            role: r.id().to_owned(),
            text: match r {
                Role::Viewer => "Browse, search, and download everything the reader role can see.",
                Role::Uploader => "Upload documents into a content bundle.",
                Role::Editor => "Edit or delete documents (they go back to review).",
                Role::Approver => "Approve documents or send them back with a note.",
                Role::Admin => "Manage people, MCP tokens, and bundles.",
            }
            .to_owned(),
            held: role.allows(*r),
        })
        .collect()
}

/// The profile queries are bounded to this many rows each.
const PROFILE_ROWS: i64 = 50;

async fn render_profile(
    app: &App,
    session: &Session,
    person: &Principal,
    notice: Option<String>,
    error: Option<String>,
) -> PageResult {
    let actor = person.actor();
    // Their row, in the users mode: whether a password or a provider signs
    // them in.
    let record = match app.auth.users() {
        Some(users) => users.record(&person.subject).await?,
        None => None,
    };
    let brought_by = match (
        app.auth.users(),
        record.as_ref().and_then(|r| r.provider.clone()),
    ) {
        (Some(users), Some(id)) => match users.provider_registry() {
            Some(registry) => registry
                .setting(&id)
                .await?
                .map_or(id, |settings| settings.provider_name),
            None => id,
        },
        _ => String::new(),
    };
    html(&ProfilePage {
        shell: Shell::new(app, session, "Your profile", "profile"),
        display: person.display.clone(),
        subject: person.subject.clone(),
        role: person.role.id().to_owned(),
        actor: actor.clone(),
        how: match session.mode {
            Mode::Users if brought_by.is_empty() => {
                "this site's own sign-in (a password kept in the catalog)".to_owned()
            }
            Mode::Users => format!("{brought_by}, an identity provider set up on the Admin page"),
            Mode::Header => "the identity provider in front of this site".to_owned(),
            Mode::Oidc => app
                .auth
                .provider(None)
                .await
                .ok()
                .flatten()
                .map_or_else(String::new, |o| o.provider_name().to_owned()),
            Mode::None => String::new(),
        },
        permissions: permission_views(person.role),
        can_change_password: record.as_ref().is_some_and(|r| r.hash.is_some()),
        sessions_revocable: app
            .auth
            .sessions()
            .is_some_and(crate::auth::Sessions::revocable),
        session_count: match app.auth.sessions() {
            Some(sessions) => sessions.session_count_for(&person.subject).await?,
            None => None,
        },
        produced: app.db.produced_by(&actor, PROFILE_ROWS).await?,
        verified: app.db.verified_by(&actor, PROFILE_ROWS).await?,
        notice,
        error,
    })
}

#[derive(Debug, Default, Deserialize)]
struct NoticeParams {
    #[serde(default)]
    notice: String,
}

async fn profile_page(
    State(app): State<Shared>,
    session: Session,
    Query(params): Query<NoticeParams>,
) -> PageResult {
    let person = signed_in(&session, "/profile")?;
    render_profile(&app, &session, &person, non_empty(&params.notice), None).await
}

/// "Sign out everywhere": end every session the person holds, this one
/// included, so a cookie copied to another device stops working now.
async fn profile_sessions_end(State(app): State<Shared>, session: Session) -> PageResult {
    let person = signed_in(&session, "/profile")?;
    let sessions = app
        .auth
        .sessions()
        .ok_or_else(|| AppError::not_found("This page"))?;
    sessions.end_all_sessions_of(&person.subject).await?;
    eprintln!(
        "pgokf-web: {} ended every session they held",
        person.actor()
    );
    let mut response = redirect("/login?next=%2Fprofile");
    for cookie in [sessions.clear_session(), sessions.clear_flow()] {
        if let Some(value) = cookie_header(&cookie) {
            response.headers_mut().append(header::SET_COOKIE, value);
        }
    }
    Ok(response)
}

#[derive(Debug, Deserialize)]
struct PasswordForm {
    #[serde(default)]
    current: String,
    #[serde(default)]
    new: String,
    #[serde(default)]
    again: String,
}

async fn profile_password(
    State(app): State<Shared>,
    session: Session,
    Form(form): Form<PasswordForm>,
) -> PageResult {
    let person = signed_in(&session, "/profile")?;
    let users = app
        .auth
        .users()
        .ok_or_else(|| AppError::not_found("This page"))?;
    let problem = if users
        .verify(&person.subject, &form.current, session.peer)
        .await?
        .is_none()
    {
        Some("The current password is wrong.".to_owned())
    } else if form.new != form.again {
        Some("The new passwords do not match.".to_owned())
    } else {
        users
            .set_password(&person.subject, &form.new)
            .await
            .err()
            .map(|e| e.to_string())
    };
    match problem {
        None => {
            // The session's binding is derived from the password hash, so
            // the change just invalidated this very session. Issue a fresh
            // cookie with it, or the person is bounced to the sign-in page
            // and never sees that it worked.
            let mut response = redirect(&format!(
                "/profile?notice={}",
                filters::percent_encode("Password changed.")
            ));
            // The change itself succeeded; a cookie that cannot be issued is
            // an honest error, not a silent bounce to the sign-in page.
            let cookie = users.issue_cookie(&person).await?;
            if let Some(value) = cookie_header(&cookie) {
                response.headers_mut().insert(header::SET_COOKIE, value);
            }
            Ok(response)
        }
        Some(problem) => {
            let mut response = render_profile(&app, &session, &person, None, Some(problem)).await?;
            *response.status_mut() = StatusCode::BAD_REQUEST;
            Ok(response)
        }
    }
}

/// The admin page needs the admin role; nothing else.
fn admin(session: &Session) -> Result<Principal, AppError> {
    let person = signed_in(session, "/admin")?;
    if person.role.allows(Role::Admin) {
        Ok(person)
    } else {
        Err(AppError::forbidden(Role::Admin))
    }
}

/// A response that carries a secret shown once: no cache - the browser's,
/// a proxy's - may keep it.
fn shown_once(mut response: Response) -> Response {
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

/// A page rendered as the refusal of the last action: a 400 carrying it.
fn as_refusal<T: Template>(page: &T) -> PageResult {
    let mut response = html(page)?;
    *response.status_mut() = StatusCode::BAD_REQUEST;
    Ok(response)
}

/// `/admin` is the first tab.
async fn admin_home(session: Session) -> PageResult {
    admin(&session)?;
    Ok(redirect(AdminTab::People.href()))
}

// ---- People ---------------------------------------------------------------

/// What the People tab is asked for: a search, a page size, a page - and a
/// notice from the action that led here.
#[derive(Debug, Default, Deserialize)]
struct PeopleParams {
    #[serde(default)]
    q: String,
    #[serde(default)]
    per: String,
    #[serde(default)]
    page: String,
    #[serde(default)]
    notice: String,
}

impl PeopleParams {
    /// The page these parameters name, once the total is known.
    fn nav(&self, total: usize) -> PeopleNav {
        let per = self
            .per
            .trim()
            .parse()
            .ok()
            .filter(|per| PEOPLE_PER_PAGE.contains(per))
            .unwrap_or(PEOPLE_PER_PAGE_DEFAULT);
        let page = self.page.trim().parse().unwrap_or(1);
        // A search longer than any name is cut, not refused.
        let search: String = self.q.trim().chars().take(128).collect();
        PeopleNav::new(search, per, page, total)
    }
}

/// The People tab, read from the catalog.
async fn render_people(
    app: &App,
    session: &Session,
    person: &Principal,
    params: &PeopleParams,
    outcome: AdminOutcome,
) -> Result<AdminPeoplePage, AppError> {
    let (people, nav) = match app.auth.users() {
        Some(users) => {
            // The page asked for, against the total: a page past the end
            // is the last one, read again.
            let asked = params.nav(usize::MAX);
            let mut page = users.people(&asked.query()).await?;
            let mut nav = params.nav(page.total);
            if nav.page != asked.page {
                page = users.people(&nav.query()).await?;
                nav = params.nav(page.total);
            }
            // Provider slugs read as the providers' names.
            let names: HashMap<String, String> = match users.provider_registry() {
                Some(registry) => registry
                    .settings()
                    .await?
                    .into_iter()
                    .map(|s| (s.id, s.provider_name))
                    .collect(),
                None => HashMap::new(),
            };
            let people = page
                .people
                .into_iter()
                .map(|u| AdminUserView {
                    is_me: u.name == person.subject,
                    provider: u.provider.map(|id| names.get(&id).cloned().unwrap_or(id)),
                    role: u.role.id().to_owned(),
                    display: u.display,
                    name: u.name,
                })
                .collect();
            (people, nav)
        }
        None => (Vec::new(), params.nav(0)),
    };
    let live_sessions: Vec<LiveSubject> = match app.auth.sessions() {
        Some(sessions) if app.auth.users().is_none() => sessions
            .live_subjects()
            .await?
            .into_iter()
            .map(|(subject, count)| LiveSubject {
                is_me: subject == person.subject,
                subject,
                count,
            })
            .collect(),
        _ => Vec::new(),
    };
    Ok(AdminPeoplePage {
        shell: Shell::new(app, session, &AdminTab::People.title(), "admin"),
        admin: AdminTab::People.shell(outcome),
        users_managed_here: app.auth.users().is_some(),
        sessions_revocable: app
            .auth
            .sessions()
            .is_some_and(crate::auth::Sessions::revocable),
        people,
        nav,
        roles: Role::all().iter().map(|r| r.id().to_owned()).collect(),
        live_sessions,
    })
}

async fn admin_people_page(
    State(app): State<Shared>,
    session: Session,
    Query(params): Query<PeopleParams>,
) -> PageResult {
    let person = admin(&session)?;
    let outcome = AdminOutcome {
        notice: non_empty(&params.notice),
        error: None,
    };
    html(&render_people(&app, &session, &person, &params, outcome).await?)
}

/// The People tab again, saying what was wrong with the last action, as a
/// 400 - on the page the action came from.
async fn people_refused(
    app: &App,
    session: &Session,
    person: &Principal,
    params: &PeopleParams,
    error: String,
) -> PageResult {
    let outcome = AdminOutcome {
        notice: None,
        error: Some(error),
    };
    as_refusal(&render_people(app, session, person, params, outcome).await?)
}

/// Back to the People tab, at the place the action came from.
fn people_redirect(params: &PeopleParams, notice: &str) -> Response {
    let nav = params.nav(usize::MAX);
    redirect(&format!(
        "{}&notice={}",
        PeopleNav::href(&nav.search, nav.per, nav.page),
        filters::percent_encode(notice)
    ))
}

#[derive(Debug, Deserialize)]
struct AdminUserForm {
    #[serde(default)]
    action: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    display: String,
    #[serde(default)]
    role: String,
    #[serde(default)]
    password: String,
    /// Where on the People tab the form was, to go back there.
    #[serde(default)]
    q: String,
    #[serde(default)]
    per: String,
    #[serde(default)]
    page: String,
}

impl AdminUserForm {
    fn place(&self) -> PeopleParams {
        PeopleParams {
            q: self.q.clone(),
            per: self.per.clone(),
            page: self.page.clone(),
            notice: String::new(),
        }
    }
}

/// One change to the people in the catalog, as the admin form asks for it.
async fn change_user(
    users: &crate::auth::UsersAuth,
    person: &Principal,
    form: &AdminUserForm,
) -> anyhow::Result<String> {
    let name = form.name.trim();
    match form.action.as_str() {
        "add" => {
            let role = Role::parse(&form.role).ok_or_else(|| anyhow::anyhow!("choose a role"))?;
            users
                .add_user(
                    name,
                    role,
                    non_empty(&form.display).as_deref(),
                    &form.password,
                )
                .await?;
            Ok(format!("Added {name}."))
        }
        "role" => {
            let role = Role::parse(&form.role).ok_or_else(|| anyhow::anyhow!("choose a role"))?;
            if name == person.subject && !role.allows(Role::Admin) {
                anyhow::bail!("you cannot take the admin role from yourself");
            }
            users.set_role(name, role).await?;
            Ok(format!("{name} is now {}.", role.id()))
        }
        "password" => {
            users.set_password(name, &form.password).await?;
            Ok(format!("Password reset for {name}."))
        }
        "remove" => {
            if name == person.subject {
                anyhow::bail!("you cannot remove yourself");
            }
            users.remove_user(name).await?;
            Ok(format!("Removed {name}."))
        }
        other => anyhow::bail!("unknown action {other:?}"),
    }
}

#[derive(Debug, Deserialize)]
struct AdminSessionsForm {
    #[serde(default)]
    name: String,
    /// Where on the People tab the form was, to go back there.
    #[serde(default)]
    q: String,
    #[serde(default)]
    per: String,
    #[serde(default)]
    page: String,
}

impl AdminSessionsForm {
    fn place(&self) -> PeopleParams {
        PeopleParams {
            q: self.q.clone(),
            per: self.per.clone(),
            page: self.page.clone(),
            notice: String::new(),
        }
    }
}

/// End every session one person holds, whichever mode identified them:
/// the lever for a cookie that may have been copied, or for someone
/// disabled at the identity provider whose session here would otherwise
/// last until it expired.
async fn admin_sessions_end(
    State(app): State<Shared>,
    session: Session,
    Form(form): Form<AdminSessionsForm>,
) -> PageResult {
    let person = admin(&session)?;
    let sessions = app.auth.sessions().ok_or_else(|| {
        AppError::bad_request("This site holds no sessions in this identity mode.")
    })?;
    let name = form.name.trim();
    if !crate::auth::valid_subject(name) {
        return people_refused(
            &app,
            &session,
            &person,
            &form.place(),
            "Name the person to sign out, as their subject (one plain token).".to_owned(),
        )
        .await;
    }
    sessions.end_all_sessions_of(name).await?;
    eprintln!(
        "pgokf-web: {} ended every session of {name}",
        person.actor()
    );
    Ok(people_redirect(
        &form.place(),
        &format!("Ended every session of {name}."),
    ))
}

async fn admin_users(
    State(app): State<Shared>,
    session: Session,
    Form(form): Form<AdminUserForm>,
) -> PageResult {
    let person = admin(&session)?;
    let users = app.auth.users().ok_or_else(|| {
        AppError::bad_request("People are managed by the identity provider, not here.")
    })?;
    match change_user(users, &person, &form).await {
        Ok(notice) => {
            eprintln!(
                "pgokf-web: {} {} user {}",
                person.actor(),
                form.action,
                form.name.trim()
            );
            Ok(people_redirect(&form.place(), &notice))
        }
        Err(error) => {
            people_refused(&app, &session, &person, &form.place(), error.to_string()).await
        }
    }
}

// ---- Identity providers ---------------------------------------------------

/// The registry, in the mode that has one.
fn provider_registry(app: &App) -> Option<&crate::provider::ProviderRegistry> {
    app.auth
        .users()
        .and_then(crate::auth::UsersAuth::provider_registry)
}

/// The list of providers, read from the catalog; each is built here as it
/// would be for a sign-in, so one this instance cannot use says so.
async fn render_providers(
    app: &App,
    session: &Session,
    outcome: AdminOutcome,
) -> Result<AdminProvidersPage, AppError> {
    let registry = provider_registry(app);
    let providers = match registry {
        Some(registry) => {
            let all = registry.settings().await?;
            registry.all_current().await?;
            all.into_iter()
                .map(|s| ProviderRow {
                    trouble: registry.trouble(&s.id),
                    id: s.id,
                    name: s.provider_name,
                    kind: s.kind.label().to_owned(),
                    issuer: s.issuer,
                    enabled: s.enabled,
                    has_secret: s.client_secret.is_some(),
                    updated_at: s.updated_at,
                    updated_by: s.updated_by,
                })
                .collect()
        }
        None => Vec::new(),
    };
    Ok(AdminProvidersPage {
        shell: Shell::new(app, session, &AdminTab::Providers.title(), "admin"),
        admin: AdminTab::Providers.shell(outcome),
        providers_managed_here: registry.is_some(),
        providers,
        can_seal: registry.is_some_and(crate::provider::ProviderRegistry::can_seal),
    })
}

async fn admin_providers_page(
    State(app): State<Shared>,
    session: Session,
    Query(params): Query<NoticeParams>,
) -> PageResult {
    admin(&session)?;
    let outcome = AdminOutcome {
        notice: non_empty(&params.notice),
        error: None,
    };
    html(&render_providers(&app, &session, outcome).await?)
}

/// The form for one provider: as stored, or blank for a new one.
async fn render_provider(
    app: &App,
    session: &Session,
    id: Option<&str>,
    outcome: AdminOutcome,
) -> Result<AdminProviderPage, AppError> {
    let registry = provider_registry(app).ok_or_else(|| {
        AppError::bad_request("Identity providers are set up here in the users mode only.")
    })?;
    let (provider, trouble) = match id {
        Some(id) => {
            let settings = registry
                .setting(id)
                .await?
                .ok_or_else(|| AppError::not_found("That identity provider"))?;
            // Read afresh, so the notice is about the settings as they
            // stand now, not as this instance last saw them.
            registry.current(id).await?;
            (ProviderView::from_settings(&settings), registry.trouble(id))
        }
        None => (ProviderView::blank(), None),
    };
    let title = match id {
        Some(_) => format!("Administration · {}", provider.provider_name),
        None => "Administration · Add an identity provider".to_owned(),
    };
    Ok(AdminProviderPage {
        shell: Shell::new(app, session, &title, "admin"),
        admin: AdminTab::Providers.shell(outcome),
        provider,
        can_seal: registry.can_seal(),
        trouble,
        roles: Role::all().iter().map(|r| r.id().to_owned()).collect(),
    })
}

async fn admin_provider_new_page(State(app): State<Shared>, session: Session) -> PageResult {
    admin(&session)?;
    html(&render_provider(&app, &session, None, AdminOutcome::default()).await?)
}

async fn admin_provider_edit_page(
    State(app): State<Shared>,
    session: Session,
    Path(id): Path<String>,
) -> PageResult {
    admin(&session)?;
    if !crate::provider_settings::valid_slug(&id) {
        return Err(AppError::not_found("That identity provider"));
    }
    html(&render_provider(&app, &session, Some(&id), AdminOutcome::default()).await?)
}

// No `Debug`: the form carries a client secret.
#[derive(Deserialize)]
struct ProviderForm {
    #[serde(default)]
    action: String,
    /// The slug of the provider being changed; empty for a new one.
    #[serde(default)]
    id: String,
    #[serde(default)]
    enabled: String,
    #[serde(default)]
    kind: String,
    #[serde(default)]
    issuer: String,
    #[serde(default)]
    client_id: String,
    #[serde(default)]
    client_secret: String,
    #[serde(default)]
    clear_secret: String,
    #[serde(default)]
    redirect_url: String,
    #[serde(default)]
    scopes: String,
    #[serde(default)]
    subject_claims: String,
    #[serde(default)]
    groups_claim: String,
    #[serde(default)]
    provider_name: String,
    #[serde(default)]
    role_map: String,
    #[serde(default)]
    default_role: String,
}

/// Add, change, or remove an identity provider people may sign in with.
/// Saving reaches the provider first (its discovery document, or GitHub's
/// API), so what is stored is known to name a provider that answers;
/// switching one off or removing it ends the sessions it opened, and
/// re-registering it as another provider also puts the people it brought
/// back at the bottom of the ladder.
async fn admin_provider(
    State(app): State<Shared>,
    session: Session,
    Form(form): Form<ProviderForm>,
) -> PageResult {
    let person = admin(&session)?;
    let registry = provider_registry(&app).ok_or_else(|| {
        AppError::bad_request("Identity providers are set up here in the users mode only.")
    })?;
    let users = app
        .auth
        .users()
        .ok_or_else(|| AppError::bad_request("This site keeps no people."))?;
    let sessions = app
        .auth
        .sessions()
        .ok_or_else(|| AppError::bad_request("This site holds no sessions."))?;
    let id = non_empty(&form.id);
    if id
        .as_deref()
        .is_some_and(|id| !crate::provider_settings::valid_slug(id))
    {
        return Err(AppError::not_found("That identity provider"));
    }
    // The form again, saying what was wrong.
    let refused = async |error: String| -> PageResult {
        let outcome = AdminOutcome {
            notice: None,
            error: Some(error),
        };
        as_refusal(&render_provider(&app, &session, id.as_deref(), outcome).await?)
    };
    let notice = match form.action.as_str() {
        "save" => {
            match save_provider(&person, registry, users, sessions, &form, id.as_deref()).await? {
                Ok(notice) => notice,
                Err(why) => return refused(why).await,
            }
        }
        "remove" => {
            let Some(id) = id.as_deref() else {
                return Err(AppError::bad_request("Name the provider to remove."));
            };
            let Some(removed) = registry.setting(id).await? else {
                return Err(AppError::not_found("That identity provider"));
            };
            registry.remove(id).await?;
            sessions.end_sessions_opened_by_provider(id).await?;
            eprintln!(
                "pgokf-web: {} removed the identity provider {id} ({})",
                person.actor(),
                removed.provider_name
            );
            format!(
                "Removed {}; the sessions it had opened are ended. The people it brought keep \
                 their rows, and sign in again once it is set up again.",
                removed.provider_name
            )
        }
        other => return Err(AppError::bad_request(format!("Unknown action {other:?}."))),
    };
    Ok(AdminTab::Providers.redirect_with(&notice))
}

/// Save a provider from the form: the notice to show, or - as the inner
/// `Err` - what was wrong, for the form to show again.
async fn save_provider(
    person: &Principal,
    registry: &crate::provider::ProviderRegistry,
    users: &crate::auth::UsersAuth,
    sessions: &crate::auth::Sessions,
    form: &ProviderForm,
    id: Option<&str>,
) -> Result<Result<String, String>, AppError> {
    let draft = match provider_draft(form) {
        Ok(draft) => draft,
        Err(why) => return Ok(Err(why.to_owned())),
    };
    let all = registry.settings().await?;
    let stored = id.and_then(|id| all.iter().find(|s| s.id == id)).cloned();
    let (settings, provider) = match registry.prepare(&draft, &all, &person.subject) {
        Ok(prepared) => prepared,
        Err(error) => return Ok(Err(format!("{error:#}."))),
    };
    if let Err(error) = provider.probe().await {
        eprintln!(
            "pgokf-web: the identity provider at {} did not answer: {error:#}",
            provider.issuer()
        );
        return Ok(Err(format!(
            "{} did not answer at {}: {error:#}. Nothing was saved.",
            provider.provider_name(),
            provider.issuer()
        )));
    }
    // Another provider, or this one re-registered, is not the one that
    // signed anyone in: those sessions end now, everywhere, and a role
    // granted to its people is not lent on to whoever the new one calls by
    // the same names.
    let changed_provider = stored.as_ref().is_some_and(|before| {
        before.kind != settings.kind
            || before.issuer.trim_end_matches('/') != settings.issuer.trim_end_matches('/')
            || before.client_id != settings.client_id
    });
    let is_new = stored.is_none();
    let saved = registry.store(&settings, provider, is_new).await?;
    if !saved.enabled || changed_provider {
        sessions.end_sessions_opened_by_provider(&saved.id).await?;
    }
    // A new provider never inherits a role: rows left under this handle by
    // a provider that once had it (removed, then this one added under the
    // same slug) go back to the bottom of the ladder, as does everyone when
    // an existing provider is re-registered as another.
    if is_new || changed_provider {
        let demoted = users.demote_people_of(&saved.id).await?;
        if demoted > 0 {
            eprintln!(
                "pgokf-web: {demoted} people under the handle {} are viewers again: it now \
                 belongs to a different provider registration",
                saved.id
            );
        }
    }
    eprintln!(
        "pgokf-web: {} {} the identity provider {} ({}, {} at {}, {})",
        person.actor(),
        if stored.is_some() { "changed" } else { "added" },
        saved.id,
        saved.provider_name,
        saved.kind.id(),
        saved.issuer,
        if saved.enabled { "on" } else { "off" }
    );
    Ok(Ok(format!(
        "Saved: {} answers at {}.{}",
        saved.provider_name,
        saved.issuer,
        if saved.enabled {
            " People can sign in with it now."
        } else {
            " It is off until you enable it; sessions it had opened are ended."
        }
    )))
}

/// The form as a draft, or what is wrong with it before anything is read.
fn provider_draft(form: &ProviderForm) -> Result<ProviderDraft, &'static str> {
    let default_role =
        Role::parse(&form.default_role).ok_or("Choose the role of everyone else.")?;
    let kind = ProviderKind::parse(&form.kind).ok_or("Choose what the provider speaks.")?;
    let client_secret = if form.clear_secret == "1" {
        SecretChange::Clear
    } else if form.client_secret.is_empty() {
        SecretChange::Keep
    } else {
        SecretChange::Set(form.client_secret.clone())
    };
    Ok(ProviderDraft {
        id: non_empty(&form.id),
        enabled: form.enabled == "1",
        kind,
        issuer: form.issuer.clone(),
        client_id: form.client_id.clone(),
        client_secret,
        redirect_url: form.redirect_url.clone(),
        scopes: form.scopes.clone(),
        subject_claims: form.subject_claims.clone(),
        groups_claim: form.groups_claim.clone(),
        provider_name: form.provider_name.clone(),
        role_map: form.role_map.clone(),
        default_role,
    })
}

// ---- MCP tokens -----------------------------------------------------------

/// The MCP tokens tab, read from the catalog.
async fn render_tokens(
    app: &App,
    session: &Session,
    outcome: AdminOutcome,
) -> Result<AdminTokensPage, AppError> {
    Ok(AdminTokensPage {
        shell: Shell::new(app, session, &AdminTab::Tokens.title(), "admin"),
        admin: AdminTab::Tokens.shell(outcome),
        mcp_tokens: match &app.mcp_tokens {
            Some(tokens) => tokens.list().await?,
            None => Vec::new(),
        },
        mcp_tokens_managed_here: app.mcp_tokens.is_some(),
        mcp_tenant: app
            .mcp_tokens
            .as_ref()
            .and_then(|tokens| tokens.tenant().map(str::to_owned)),
        mcp_roles: McpRole::all().iter().map(|r| r.id().to_owned()).collect(),
        minted: None,
    })
}

async fn admin_tokens_page(
    State(app): State<Shared>,
    session: Session,
    Query(params): Query<NoticeParams>,
) -> PageResult {
    admin(&session)?;
    let outcome = AdminOutcome {
        notice: non_empty(&params.notice),
        error: None,
    };
    html(&render_tokens(&app, &session, outcome).await?)
}

// ---- Bundles and settings -------------------------------------------------

async fn admin_bundles_page(
    State(app): State<Shared>,
    session: Session,
    Query(params): Query<NoticeParams>,
) -> PageResult {
    admin(&session)?;
    html(&AdminBundlesPage {
        shell: Shell::new(&app, &session, &AdminTab::Bundles.title(), "admin"),
        admin: AdminTab::Bundles.shell(AdminOutcome {
            notice: non_empty(&params.notice),
            error: None,
        }),
        bundles: app.db.admin_bundles().await?,
    })
}

async fn admin_settings_page(State(app): State<Shared>, session: Session) -> PageResult {
    admin(&session)?;
    html(&AdminSettingsPage {
        shell: Shell::new(&app, &session, &AdminTab::Settings.title(), "admin"),
        admin: AdminTab::Settings.shell(AdminOutcome::default()),
        config_json: serde_json::to_string_pretty(&app.db.config().await?).unwrap_or_default(),
    })
}

#[derive(Debug, Deserialize)]
struct AdminTokenForm {
    #[serde(default)]
    action: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    role: String,
}

/// Mint or revoke an MCP bearer token. A minted token is shown on this
/// response alone - rendered directly rather than after a redirect, so it
/// never enters a URL, a log, or a referer - and the response is marked
/// `no-store`, so no cache keeps it either.
async fn admin_mcp_tokens(
    State(app): State<Shared>,
    session: Session,
    Form(form): Form<AdminTokenForm>,
) -> PageResult {
    let person = admin(&session)?;
    let tokens = app.mcp_tokens.as_ref().ok_or_else(|| {
        AppError::bad_request("MCP tokens need the writer connection (OKF_PG_WRITER_URL).")
    })?;
    let refused = async |error: String| -> PageResult {
        let outcome = AdminOutcome {
            notice: None,
            error: Some(error),
        };
        as_refusal(&render_tokens(&app, &session, outcome).await?)
    };
    let name = form.name.trim();
    match form.action.as_str() {
        "mint" => {
            let Some(role) = McpRole::parse(&form.role) else {
                return refused("Choose a role: reader or builder.".to_owned()).await;
            };
            if valid_token_name(name).is_err() {
                return refused(
                    "Name the token: letters, digits, and . _ - @ + (at most 128).".to_owned(),
                )
                .await;
            }
            // Everything the page needs is read *before* the token is
            // minted, so a catalog that fails afterwards cannot leave a
            // token minted and never shown; the new row is added by hand.
            let mut page = render_tokens(&app, &session, AdminOutcome::default()).await?;
            let (token, record) = match tokens.mint(name, role, &person.subject).await? {
                Minted::Token { token, record } => (token, record),
                Minted::NameTaken => {
                    page.admin.error = Some(format!(
                        "A token named {name} already exists; revoke it first, or choose another name."
                    ));
                    return as_refusal(&page);
                }
                // Checked above; the service checks again on its own account.
                Minted::InvalidName(why) => return Err(AppError::bad_request(why)),
            };
            eprintln!(
                "pgokf-web: {} minted MCP token {name} ({role})",
                person.actor()
            );
            page.mcp_tokens.insert(0, record);
            page.minted = Some(MintedToken {
                name: name.to_owned(),
                role,
                token,
            });
            Ok(shown_once(html(&page)?))
        }
        "revoke" => {
            if valid_token_name(name).is_err() {
                return refused("Name the token to revoke.".to_owned()).await;
            }
            if !tokens.revoke(name).await? {
                return refused(format!("No token is named {name}.")).await;
            }
            eprintln!("pgokf-web: {} revoked MCP token {name}", person.actor());
            Ok(AdminTab::Tokens.redirect_with(&format!(
                "Revoked the MCP token {name}; every request it makes is refused from now on."
            )))
        }
        other => Err(AppError::bad_request(format!("Unknown action {other:?}."))),
    }
}

#[derive(Debug, Deserialize)]
struct AdminBundleForm {
    #[serde(default)]
    action: String,
    #[serde(default)]
    id: String,
    #[serde(default)]
    path: String,
    #[serde(default)]
    name: String,
}

async fn admin_bundles(
    State(app): State<Shared>,
    session: Session,
    Form(form): Form<AdminBundleForm>,
) -> PageResult {
    let person = admin(&session)?;
    let access = require(&app, &session, Role::Admin, "/admin")?;
    let id = || -> Result<i64, AppError> {
        form.id
            .trim()
            .parse()
            .map_err(|_| AppError::bad_request("Choose a bundle."))
    };
    let notice = match form.action.as_str() {
        "refresh" => {
            let _one_at_a_time = app.rebuilds.lock().await;
            let outcome = access.writer.refresh_bundle(id()?).await?;
            format!(
                "Refreshed bundle {}: {} added, {} updated, {} removed.",
                outcome.bundle_id, outcome.added, outcome.updated, outcome.removed
            )
        }
        "enable" | "disable" => {
            let enabled = form.action == "enable";
            access.writer.set_bundle_enabled(id()?, enabled).await?;
            format!(
                "Bundle {} {}.",
                form.id.trim(),
                if enabled { "enabled" } else { "disabled" }
            )
        }
        "retire" | "unretire" => {
            let retire = form.action == "retire";
            access.writer.retire_bundle(id()?, retire).await?;
            format!(
                "Bundle {} {}.",
                form.id.trim(),
                if retire { "retired" } else { "brought back" }
            )
        }
        "unregister" => {
            access.writer.unregister_bundle(id()?).await?;
            format!("Bundle {} unregistered.", form.id.trim())
        }
        "register" => {
            let path = form.path.trim();
            if path.is_empty() {
                return Err(AppError::bad_request("Give the bundle's path."));
            }
            let _one_at_a_time = app.rebuilds.lock().await;
            let outcome = access
                .writer
                .register_bundle(path, non_empty(&form.name).as_deref())
                .await?;
            format!(
                "Registered bundle {} with {} concept{}.",
                outcome.bundle_id,
                outcome.added,
                if outcome.added == 1 { "" } else { "s" }
            )
        }
        other => return Err(AppError::bad_request(format!("Unknown action {other:?}."))),
    };
    eprintln!(
        "pgokf-web: {} {} bundle {}{}",
        person.actor(),
        form.action,
        form.id.trim(),
        if form.path.trim().is_empty() {
            String::new()
        } else {
            format!(" at {}", form.path.trim())
        }
    );
    Ok(AdminTab::Bundles.redirect_with(&notice))
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

/// Where the unpacked zip goes: an Agent Plugin is one directory the client
/// installs, every other shape is unpacked over the workspace root.
fn install_note(profile: &Profile<'_>, name: &str, remote: bool) -> String {
    if profile.shape != pgokf_workspace::Shape::AgentPlugin {
        return "Unpack at the workspace root.".to_owned();
    }
    let credential = if remote {
        "the specification forbids a credential in a package, so give your client the endpoint's \
         bearer token itself"
    } else {
        "write OKF_PG_URL into pgokf.env under the client's plugin data directory"
    };
    format!(
        "Unpack anywhere and install the {name}/ directory with your agent's plugin command \
         (a plugin.json, skills/, and mcp.json as the Agent Plugins Specification defines \
         them); then, if the MCP entry is included, {credential}."
    )
}

/// Every concept of one bundle for the builder's file picker (capped; the
/// response says when the cap was hit).
async fn api_bundle_tree(
    State(app): State<Shared>,
    Path(id): Path<i64>,
) -> Result<Json<Value>, AppError> {
    let (entries, truncated) = capped(app.db.bundle_tree(id, TREE_CAP + 1).await?, TREE_CAP);
    Ok(Json(serde_json::json!({
        "bundle_id": id,
        "truncated": truncated,
        "entries": entries,
    })))
}

/// Keep at most `cap` rows of a listing fetched with one extra row, and say
/// whether that extra row existed (the listing was cut).
fn capped<T>(mut rows: Vec<T>, cap: i64) -> (Vec<T>, bool) {
    let cap = usize::try_from(cap).unwrap_or(usize::MAX);
    let truncated = rows.len() > cap;
    rows.truncate(cap);
    (rows, truncated)
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
    Ok(Json(graph_json(
        Some((bundle_id, &concept_id)),
        hops,
        &graph,
        ColorBy::Hops,
    )))
}

/// Nodes drawn by the catalog-wide explorer at most, and by default.
const GRAPH_MAX_NODES: i32 = 2000;
const GRAPH_DEFAULT_NODES: i32 = 300;
const GRAPH_NODE_OPTIONS: [i32; 5] = [100, 300, 600, 1000, 2000];

#[derive(Debug, Default, Deserialize)]
struct CatalogGraphParams {
    #[serde(default)]
    bundle: String,
    #[serde(default)]
    limit: String,
    /// `bundle_id:concept_id` to draw a neighborhood instead.
    #[serde(default)]
    seed: String,
    #[serde(default)]
    hops: String,
}

impl CatalogGraphParams {
    fn bundle_id(&self) -> Result<Option<i64>, AppError> {
        match non_empty(&self.bundle) {
            None => Ok(None),
            Some(raw) => raw
                .parse::<i64>()
                .map(Some)
                .map_err(|_| AppError::bad_request("bundle must be an integer id")),
        }
    }

    fn limit(&self) -> i32 {
        self.limit
            .parse::<i32>()
            .ok()
            .filter(|n| (1..=GRAPH_MAX_NODES).contains(n))
            .unwrap_or(GRAPH_DEFAULT_NODES)
    }

    fn seed(&self) -> Result<Option<(i64, String)>, AppError> {
        match non_empty(&self.seed) {
            None => Ok(None),
            Some(raw) => match raw.split_once(':') {
                Some((b, id)) if !id.is_empty() => Ok(Some((
                    b.parse::<i64>()
                        .map_err(|_| AppError::bad_request("seed must be bundle_id:concept_id"))?,
                    id.to_owned(),
                ))),
                _ => Err(AppError::bad_request("seed must be bundle_id:concept_id")),
            },
        }
    }
}

/// The catalog-wide graph (or a seeded neighborhood) for the explorer.
async fn api_catalog_graph(
    State(app): State<Shared>,
    Query(params): Query<CatalogGraphParams>,
) -> Result<Json<Value>, AppError> {
    let bundle_id = params.bundle_id()?;
    if let Some((seed_bundle, seed_id)) = params.seed()? {
        let hops = parse_hops(&params.hops);
        let graph = app.db.graph(seed_bundle, &seed_id, hops).await?;
        if graph.nodes.is_empty() {
            return Err(AppError::not_found("This concept"));
        }
        return Ok(Json(graph_json(
            Some((seed_bundle, &seed_id)),
            hops,
            &graph,
            ColorBy::Hops,
        )));
    }
    let graph = app
        .db
        .catalog_graph(bundle_id, i64::from(params.limit()))
        .await?;
    let color_by = if bundle_id.is_some() {
        ColorBy::Type
    } else {
        ColorBy::Bundle
    };
    Ok(Json(graph_json(None, 0, &graph, color_by)))
}

async fn graph_page(
    State(app): State<Shared>,
    session: Session,
    Query(params): Query<CatalogGraphParams>,
) -> PageResult {
    let bundle_id = params.bundle_id()?;
    let seed = params.seed()?;
    let limit = params.limit();
    let bundles = app.db.bundles().await?;
    let mut pairs: Vec<(String, String)> = Vec::new();
    if let Some(b) = bundle_id {
        pairs.push(("bundle".to_owned(), b.to_string()));
    }
    pairs.push(("limit".to_owned(), limit.to_string()));
    let hops = parse_hops(&params.hops);
    let seed_label = match &seed {
        Some((b, id)) => {
            pairs.push(("seed".to_owned(), format!("{b}:{id}")));
            Some(
                app.db
                    .concept(*b, id)
                    .await?
                    .and_then(|c| c.title)
                    .unwrap_or_else(|| id.clone()),
            )
        }
        None => None,
    };
    let query: Vec<String> = pairs
        .iter()
        .map(|(k, v)| format!("{k}={}", filters::percent_encode(v)))
        .collect();
    html(&GraphPage {
        shell: Shell::new(&app, &session, "Graph", "graph"),
        bundles,
        bundle: bundle_id.map(|b| b.to_string()).unwrap_or_default(),
        limit_options: GRAPH_NODE_OPTIONS
            .iter()
            .map(|n| (*n, *n == limit))
            .collect(),
        graph_url: format!("/api/graph?{}", query.join("&")),
        hops,
        seed_label,
    })
}

// ---------------------------------------------------------------------------
// Plugin builder
// ---------------------------------------------------------------------------

/// Raw query-string parameters of the builder page, its preview, and the
/// download.
#[derive(Debug, Clone, Default, Deserialize)]
struct PluginParams {
    /// The older form of the choice: a target id alone.
    #[serde(default)]
    target: String,
    /// What is being built (a `Shape` id) and for which agent (a registry
    /// label or id, or a new name the user adds).
    #[serde(default)]
    kind: String,
    #[serde(default)]
    agent: String,
    /// For an added skills agent: where it reads skills from.
    #[serde(default)]
    skills_dir: String,
    /// `1` when the selection starts from everything in scope.
    #[serde(default)]
    all: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    title: String,
    #[serde(default)]
    bundle: String,
    #[serde(default)]
    types: String,
    #[serde(default)]
    tags: String,
    #[serde(default)]
    ids: String,
    /// Specific files, one `bundle_id:concept_id` per line.
    #[serde(default)]
    picks: String,
    #[serde(default)]
    q: String,
    #[serde(default)]
    verified: String,
    #[serde(default)]
    limit: String,
    #[serde(default)]
    base_model: String,
    /// Component flags (`1`/`on`), one field each: the query parser does
    /// not collect repeated keys.
    #[serde(default)]
    mcp: String,
    #[serde(default)]
    guide: String,
    #[serde(default)]
    tools: String,
    #[serde(default)]
    mcp_command: String,
    #[serde(default)]
    mcp_url: String,
    #[serde(default)]
    web_url: String,
}

const DEFAULT_PLUGIN_NAME: &str = "okf-knowledge";

/// Where an added skills agent is assumed to read skills from until the
/// user says otherwise: the cross-tool location several harnesses share.
const DEFAULT_SKILLS_DIR: &str = ".agents/skills";

/// What the builder builds for, resolved from the form's kind and agent
/// (or the older `target` id alone).
#[derive(Debug, Clone)]
struct Chosen {
    target: Target,
    harness: Option<CustomHarness>,
    kind: Shape,
    /// The agent as the chooser shows it.
    agent: String,
    skills_dir: String,
    /// Why the choice cannot be built (an invalid custom directory).
    problem: Option<String>,
}

impl Chosen {
    fn registry(profile: &'static Profile<'static>) -> Self {
        Self {
            target: profile.target,
            harness: None,
            kind: profile.shape,
            agent: short_label(profile.label).to_owned(),
            skills_dir: String::new(),
            problem: None,
        }
    }

    /// One line about the chosen agent for the page.
    fn note(&self) -> String {
        if let Some(problem) = &self.problem {
            return problem.clone();
        }
        match (&self.harness, Profile::of(self.target)) {
            (Some(harness), _) if harness.shape == Shape::Skills => format!(
                "{} is not in the list, so the skill package is laid out like the generic Agent \
                 Skills package under {}/ (change the directory if {} reads another), and the \
                 MCP entry is a snippet to merge into its own configuration.",
                harness.label,
                harness.skills_dir.as_deref().unwrap_or(DEFAULT_SKILLS_DIR),
                harness.label
            ),
            (Some(harness), _) => format!(
                "{} is not in the list; the tree is the standard {} layout, which any such agent \
                 reads.",
                harness.label,
                harness.shape.label().to_lowercase()
            ),
            (None, Some(profile)) => profile.notes.to_owned(),
            (None, None) => String::new(),
        }
    }
}

/// A registry label without its parenthetical explainer.
fn short_label(label: &str) -> &str {
    label
        .split_once(" (")
        .map_or(label, |(name, _)| name)
        .trim()
}

/// The registry agent of a kind that a typed name refers to: its id, its
/// label, or the label's short form, case-insensitively.
fn registry_agent(kind: Shape, agent: &str) -> Option<&'static Profile<'static>> {
    Profile::with_shape(kind).find(|p| {
        p.id.eq_ignore_ascii_case(agent)
            || p.label.eq_ignore_ascii_case(agent)
            || short_label(p.label).eq_ignore_ascii_case(agent)
    })
}

/// Resolve the form's choice. A kind with no agent takes the kind's first
/// registry agent; a name outside the registry adds a custom agent (for a
/// skills package, under `skills_dir` or the shared default); an invalid
/// directory is reported as a problem rather than a request error so the
/// page keeps working. A kind that is not one of the five is a 400.
fn resolve_agent(
    kind: &str,
    agent: &str,
    skills_dir: &str,
    target: &str,
) -> Result<Chosen, AppError> {
    let kind_id = kind.trim();
    let agent = agent.trim();
    let skills_dir = skills_dir.trim();
    if kind_id.is_empty() && agent.is_empty() {
        let id = non_empty(target).unwrap_or_else(|| "agent-plugin".to_owned());
        let profile = Profile::by_id(id.trim())
            .ok_or_else(|| AppError::bad_request(format!("unknown target {id}")))?;
        return Ok(Chosen::registry(profile));
    }
    let kind = if kind_id.is_empty() {
        Shape::AgentPlugin
    } else {
        Shape::parse(kind_id)
            .ok_or_else(|| AppError::bad_request(format!("unknown kind {kind_id}")))?
    };
    if agent.is_empty() {
        let first = Profile::with_shape(kind)
            .next()
            .unwrap_or_else(|| kind.base());
        return Ok(Chosen::registry(first));
    }
    if let Some(profile) = registry_agent(kind, agent) {
        return Ok(Chosen::registry(profile));
    }
    let dir = if kind == Shape::Skills && skills_dir.is_empty() {
        DEFAULT_SKILLS_DIR
    } else {
        skills_dir
    };
    match CustomHarness::new(agent, kind, Some(dir)) {
        Ok(harness) => Ok(Chosen {
            target: Target::Custom,
            kind,
            agent: harness.label.clone(),
            skills_dir: harness.skills_dir.clone().unwrap_or_default(),
            harness: Some(harness),
            problem: None,
        }),
        Err(error) => Ok(Chosen {
            target: kind.base().target,
            harness: None,
            kind,
            agent: agent.to_owned(),
            skills_dir: dir.to_owned(),
            problem: Some(format!("{agent} cannot be built for yet: {error}")),
        }),
    }
}

/// The picks field: one `bundle_id:concept_id` per line, duplicates folded.
/// A malformed line is not a request error - the preview must keep updating
/// while the user types - so it comes back as a message for the page, with
/// the well-formed lines still applied; the download refuses it.
fn parse_picks(raw: &str) -> (Vec<ConceptRef>, Option<String>) {
    let (picks, malformed) = ConceptRef::parse_entries(raw);
    let problem = malformed.first().map(|line| {
        format!(
            "picked file {line:?} is not of the form bundle:concept id (for example \
             3:skills/deploy/SKILL); tick files in the picker or fix the line"
        )
    });
    (picks, problem)
}

fn split_list(raw: &str) -> Vec<String> {
    raw.split([',', '\n'])
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .map(str::to_owned)
        .collect()
}

impl PluginParams {
    /// Validate into the builder's inputs plus the echoed form state.
    fn normalize(&self) -> Result<(PluginForm, Chosen, Selection, Option<String>), AppError> {
        let chosen = resolve_agent(&self.kind, &self.agent, &self.skills_dir, &self.target)?;
        let bundle_id = match non_empty(&self.bundle) {
            None => None,
            Some(raw) => Some(
                raw.parse::<i64>()
                    .map_err(|_| AppError::bad_request("bundle must be an integer id"))?,
            ),
        };
        let limit = match non_empty(&self.limit) {
            None => None,
            Some(raw) => Some(
                raw.parse::<usize>()
                    .ok()
                    .filter(|n| (1..=pgokf_workspace::MAX_CONCEPTS).contains(n))
                    .ok_or_else(|| {
                        AppError::bad_request(format!(
                            "limit must be between 1 and {}",
                            pgokf_workspace::MAX_CONCEPTS
                        ))
                    })?,
            ),
        };
        let on = |raw: &str| matches!(raw.trim(), "1" | "true" | "on");
        let verified = on(&self.verified);
        let all = on(&self.all);
        let components = self.components();
        let (picks, pick_problem) = parse_picks(&self.picks);
        // The rule (everything in scope, narrowed) applies only when the
        // user took everything; otherwise the bundle is just where they
        // browse and the narrowing fields wait, echoed but inert.
        let selection = Selection {
            all,
            bundle_ids: bundle_id.filter(|_| all).into_iter().collect(),
            concept_ids: if all {
                split_list(&self.ids)
            } else {
                Vec::new()
            },
            tags: if all {
                split_list(&self.tags)
            } else {
                Vec::new()
            },
            types: if all {
                split_list(&self.types)
            } else {
                Vec::new()
            },
            query: non_empty(&self.q).filter(|_| all),
            verified_only: verified,
            limit,
            picks,
        };
        let name = non_empty(&self.name).unwrap_or_else(|| DEFAULT_PLUGIN_NAME.to_owned());
        let query_string =
            self.query_string(&chosen, &name, &selection, limit, verified, &components);
        let form = PluginForm {
            kind: chosen.kind.id().to_owned(),
            agent: chosen.agent.clone(),
            skills_dir: chosen.skills_dir.clone(),
            custom: chosen.harness.is_some() || chosen.problem.is_some(),
            agent_note: chosen.note(),
            agent_problem: chosen.problem.clone(),
            all,
            selection_summary: selection.describe(),
            name,
            title: self.title.trim().to_owned(),
            bundle: bundle_id.map(|b| b.to_string()).unwrap_or_default(),
            types: split_list(&self.types).join(", "),
            tags: split_list(&self.tags).join(", "),
            ids: split_list(&self.ids).join("\n"),
            picks: selection
                .picks
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join("\n"),
            q: self.q.trim().to_owned(),
            verified,
            limit: limit.map(|n| n.to_string()).unwrap_or_default(),
            base_model: self.base_model.trim().to_owned(),
            with_mcp: components.contains(&Component::Mcp),
            with_guide: components.contains(&Component::Guide),
            with_tools: components.contains(&Component::Tools),
            mcp_command: self.mcp_command.trim().to_owned(),
            mcp_url: self.mcp_url.trim().to_owned(),
            web_url: self.web_url.trim().to_owned(),
            has_selection: !selection.is_empty(),
            query_string,
            pick_problem,
        };
        Ok((form, chosen, selection, non_empty(&self.base_model)))
    }

    /// The request as a query string, for the download link and the preview
    /// partial (empty fields dropped, one flag per chosen component).
    fn query_string(
        &self,
        chosen: &Chosen,
        name: &str,
        selection: &Selection,
        limit: Option<usize>,
        verified: bool,
        components: &[Component],
    ) -> String {
        let flag = |on: bool| if on { "1".to_owned() } else { String::new() };
        let pairs: Vec<(&str, String)> = vec![
            ("kind", chosen.kind.id().to_owned()),
            ("agent", chosen.agent.clone()),
            (
                "skills_dir",
                if chosen.target == Target::Custom || chosen.problem.is_some() {
                    chosen.skills_dir.clone()
                } else {
                    String::new()
                },
            ),
            ("name", name.to_owned()),
            ("title", self.title.trim().to_owned()),
            ("bundle", self.bundle.trim().to_owned()),
            ("all", flag(selection.all)),
            ("types", split_list(&self.types).join(", ")),
            ("tags", split_list(&self.tags).join(", ")),
            ("ids", split_list(&self.ids).join(", ")),
            (
                "picks",
                selection
                    .picks
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join("\n"),
            ),
            ("q", self.q.trim().to_owned()),
            ("verified", flag(verified)),
            ("limit", limit.map(|n| n.to_string()).unwrap_or_default()),
            ("base_model", self.base_model.trim().to_owned()),
            ("mcp_command", self.mcp_command.trim().to_owned()),
            ("mcp_url", self.mcp_url.trim().to_owned()),
            ("web_url", self.web_url.trim().to_owned()),
        ];
        let mut query: Vec<String> = pairs
            .iter()
            .filter(|(_, v)| !v.is_empty())
            .map(|(k, v)| format!("{k}={}", filters::percent_encode(v)))
            .collect();
        query.extend(components.iter().map(|c| format!("{}=1", c.id())));
        query.join("&")
    }

    /// The requested components, in canonical order.
    fn components(&self) -> Vec<Component> {
        let on = |raw: &str| matches!(raw.trim(), "1" | "true" | "on");
        [
            (on(&self.mcp), Component::Mcp),
            (on(&self.guide), Component::Guide),
            (on(&self.tools), Component::Tools),
        ]
        .into_iter()
        .filter_map(|(set, c)| set.then_some(c))
        .collect()
    }
}

/// The kinds of tree, the chosen one marked.
fn kind_views(selected: Shape) -> Vec<KindView> {
    Shape::all()
        .iter()
        .map(|shape| KindView {
            id: shape.id().to_owned(),
            label: shape.label().to_owned(),
            description: shape.description().to_owned(),
            selected: *shape == selected,
        })
        .collect()
}

/// Every registry agent under its kind, for the chooser's lists.
fn agent_views() -> Vec<AgentView> {
    Profile::all()
        .iter()
        .map(|p| AgentView {
            id: p.id.to_owned(),
            label: short_label(p.label).to_owned(),
            kind: p.shape.id().to_owned(),
            root: match p.shape {
                Shape::AgentPlugin => "<name>/plugin.json".to_owned(),
                _ if p.root.is_empty() => "workspace root".to_owned(),
                _ => format!("{}/", p.root),
            },
            notes: p.notes.to_owned(),
        })
        .collect()
}

fn build_options(
    app: &App,
    form: &PluginForm,
    chosen: &Chosen,
    base_model: Option<String>,
) -> BuildOptions {
    let mut components = Vec::new();
    if form.with_mcp {
        components.push(Component::Mcp);
    }
    if form.with_guide {
        components.push(Component::Guide);
    }
    if form.with_tools {
        components.push(Component::Tools);
    }
    BuildOptions {
        target: chosen.target,
        harness: chosen.harness.clone(),
        name: form.name.clone(),
        title: non_empty(&form.title),
        catalog_name: app.catalog_name.clone(),
        base_model,
        components,
        mcp_command: non_empty(&form.mcp_command),
        mcp_url: non_empty(&form.mcp_url),
        tenant: app.tenant.clone(),
        web_url: non_empty(&form.web_url),
    }
}

/// A builder failure as the page reports it: a catalog failure keeps its
/// classification; anything else (an empty or unmatched selection, a bad
/// name, a colliding path) is the caller's input.
fn workspace_error(error: anyhow::Error) -> AppError {
    if error
        .chain()
        .any(|cause| cause.downcast_ref::<tokio_postgres::Error>().is_some())
        || crate::db::classify(&error) != Failure::Other
    {
        error.into()
    } else {
        AppError::bad_request(format!("{error:#}"))
    }
}

/// What the preview says about the MCP entry: where it is written, and how
/// the harness reaches the catalog with it.
fn mcp_note(profile: &Profile, name: &str, remote: bool) -> String {
    let Some(spec) = profile.mcp else {
        return "This target has no MCP configuration; the guide points at the JSON API instead."
            .to_owned();
    };
    let (path, auto_loaded) = spec.location(remote);
    // A plugin's own files live inside its directory, as the file list on
    // the same page shows them.
    let path = if profile.shape == Shape::AgentPlugin {
        format!("{}/{path}", pgokf_workspace::slug(name))
    } else {
        path.to_owned()
    };
    let where_it_goes = match spec.merge_target(remote) {
        None if auto_loaded => format!("{path}, which {} loads from the workspace", profile.label),
        None => format!("{path}; merge it into the harness's own configuration"),
        Some(target) => format!("{path}; merge it into the harness's own {target}"),
    };
    if remote {
        format!(
            "The entry points {} at the endpoint you gave and is written to {where_it_goes}. \
             The bearer token is never written into the tree.",
            profile.label
        )
    } else {
        format!("The MCP server entry is written to {where_it_goes}.")
    }
}

/// Resolve the selection (no sources read) and describe the tree it would
/// produce.
async fn plugin_preview(
    app: &App,
    form: &PluginForm,
    chosen: &Chosen,
    selection: &Selection,
    base_model: Option<String>,
) -> Result<PluginPreview, AppError> {
    let mut client = app.db.checkout().await?;
    let mut concepts = pgokf_workspace::resolve(client.client(), selection)
        .await
        .map_err(workspace_error)?;
    client.finish();
    let truncated = concepts.len() >= selection.effective_limit();
    // The build drops a resource whose package is selected; the preview
    // must show the same tree.
    drop_packaged_resources(&mut concepts);
    placeholder_contents(app, &mut concepts).await?;
    let options = build_options(app, form, chosen, base_model);
    let profile = options.profile().map_err(workspace_error)?;
    let snapshot = pgokf_workspace::Snapshot {
        version: app.version.clone(),
        sql_version: String::new(),
        bundles: Vec::new(),
    };
    let plugin = if concepts.is_empty() {
        None
    } else {
        Some(
            pgokf_workspace::assemble(&options, selection, &snapshot, &concepts)
                .map_err(workspace_error)?,
        )
    };
    for c in &mut concepts {
        c.bytes.clear();
    }
    let mcp_args = serde_json::json!({
        "target": chosen.target.id(),
        "harness": chosen.harness,
        "name": pgokf_workspace::slug(&form.name),
        "all": selection.all,
        "bundle_ids": selection.bundle_ids,
        "types": selection.types,
        "tags": selection.tags,
        "concept_ids": selection.concept_ids,
        "picks": selection.picks.iter().map(ToString::to_string).collect::<Vec<_>>(),
        "query": selection.query,
        "verified_only": selection.verified_only,
        "limit": selection.effective_limit(),
        "components": options.components.iter().map(|c| c.id()).collect::<Vec<_>>(),
        "mcp_command": options.mcp_command,
        "mcp_url": options.mcp_url,
        "web_url": options.web_url,
        "output_dir": "/path/to/your/workspace",
    });
    let mcp_note = form
        .with_mcp
        .then(|| mcp_note(&profile, &form.name, !form.mcp_url.is_empty()));
    let mcp_args = match mcp_args {
        Value::Object(map) => Value::Object(
            map.into_iter()
                .filter(|(_, v)| {
                    !matches!(v, Value::Null)
                        && !v.as_array().is_some_and(Vec::is_empty)
                        && v != &Value::Bool(false)
                })
                .collect(),
        ),
        other => other,
    };
    Ok(PluginPreview {
        description: selection.describe(),
        truncated,
        root: plugin.as_ref().map(|p| p.root.clone()).unwrap_or_default(),
        index_file: plugin
            .as_ref()
            .and_then(|p| p.files.first().map(|f| f.path.clone()))
            .unwrap_or_default(),
        file_paths: plugin
            .as_ref()
            .map(|p| p.files.iter().map(|f| f.path.clone()).collect())
            .unwrap_or_default(),
        mcp_note,
        mcp_call: serde_json::to_string_pretty(&serde_json::json!({
            "name": "build_workspace_plugin",
            "arguments": mcp_args,
        }))
        .unwrap_or_default(),
        download_url: format!("/plugins/build.zip?{}", form.query_string),
        package_count: plugin.as_ref().map_or(0, |p| p.package_count),
        target_label: profile.label.to_owned(),
        install_note: install_note(&profile, &form.name, !form.mcp_url.is_empty()),
        concepts,
    })
}

/// The preview, or the message to show in its place; a selection that is
/// not set yet shows neither.
async fn preview_or_message(
    app: &App,
    form: &PluginForm,
    chosen: &Chosen,
    selection: &Selection,
    base_model: Option<String>,
) -> (Option<PluginPreview>, Option<String>) {
    if !form.has_selection || form.agent_problem.is_some() {
        return (None, None);
    }
    match plugin_preview(app, form, chosen, selection, base_model).await {
        Ok(preview) => (Some(preview), None),
        Err(error) => (None, Some(error.message)),
    }
}

async fn plugins_page(
    State(app): State<Shared>,
    session: Session,
    Query(params): Query<PluginParams>,
) -> PageResult {
    let (form, chosen, selection, base_model) = params.normalize()?;
    let (preview, error) = preview_or_message(&app, &form, &chosen, &selection, base_model).await;
    let error = form.problems().or(error);
    let bundles = app.db.bundles().await?;
    let type_facets = app.db.catalog_facets(None, "type").await?;
    let mut tag_facets = app.db.catalog_facets(None, "tag").await?;
    tag_facets.truncate(CHIP_TAGS);
    html(&PluginsPage {
        shell: Shell::new(&app, &session, "Agent Plugin builder", "plugins"),
        kinds: kind_views(chosen.kind),
        agents: agent_views(),
        form,
        bundles,
        type_facets,
        tag_facets,
        preview,
        error,
    })
}

async fn plugins_preview(
    State(app): State<Shared>,
    Query(params): Query<PluginParams>,
) -> PageResult {
    let (form, chosen, selection, base_model) = params.normalize()?;
    let (preview, error) = preview_or_message(&app, &form, &chosen, &selection, base_model).await;
    let error = form.problems().or(error);
    let push_url = format!("/plugins?{}", form.query_string);
    let mut response = html(&PluginPreviewPartial {
        form,
        preview,
        error,
    })?;
    if let Ok(value) = HeaderValue::from_str(&push_url) {
        response.headers_mut().insert("HX-Push-Url", value);
    }
    Ok(response)
}

/// The exact bytes of a package manifest, script, reference, or asset
/// through the audited readers, as an attachment (never rendered inline:
/// an HTML or SVG asset must not run as a page of this origin).
async fn concept_resource(
    State(app): State<Shared>,
    Path((bundle_id, concept_id)): Path<(i64, String)>,
) -> PageResult {
    let found = match app.db.exact_bytes(bundle_id, &concept_id).await {
        Ok(Some(found)) => found,
        Ok(None) => return Err(AppError::not_found("The stored bytes of this concept")),
        Err(error) if crate::db::classify(&error) == Failure::InvalidInput => {
            return Err(AppError::not_found("The stored bytes of this concept"));
        }
        Err(error) => return Err(error.into()),
    };
    let content_type = safe_media_type(&found.media_type);
    Ok((
        [
            (
                header::CONTENT_TYPE,
                HeaderValue::from_str(&content_type)
                    .unwrap_or_else(|_| HeaderValue::from_static("application/octet-stream")),
            ),
            (
                header::CONTENT_DISPOSITION,
                HeaderValue::from_str(&resource_disposition(&concept_id, found.is_manifest))
                    .unwrap_or_else(|_| HeaderValue::from_static("attachment")),
            ),
        ],
        found.bytes,
    )
        .into_response())
}

/// A media type from the catalog as a response header value: only the
/// `type/subtype` grammar is trusted, text types get a charset, and anything
/// else is served as an opaque stream.
fn safe_media_type(media_type: &str) -> String {
    let well_formed = media_type.len() <= 100
        && media_type.split_once('/').is_some_and(|(t, sub)| {
            !t.is_empty()
                && !sub.is_empty()
                && t.chars()
                    .chain(sub.chars())
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '+' | '-'))
        });
    if !well_formed {
        return "application/octet-stream".to_owned();
    }
    if media_type.starts_with("text/")
        || matches!(
            media_type,
            "application/json" | "application/yaml" | "application/toml" | "application/xml"
        )
    {
        format!("{media_type}; charset=utf-8")
    } else {
        media_type.to_owned()
    }
}

/// Placeholder content for a preview: the layout is what the preview shows;
/// sources are only read (and audited) by the download. A package's
/// resource paths come from the projection tables so the file list is
/// complete.
async fn placeholder_contents(app: &App, concepts: &mut [ConceptRecord]) -> Result<(), AppError> {
    let packages: Vec<(i64, String)> = concepts
        .iter()
        .filter(|c| c.package.is_some())
        .map(|c| (c.bundle_id, c.concept_id.clone()))
        .collect();
    let mut listings = if packages.is_empty() {
        BTreeMap::new()
    } else {
        app.db.package_resources(&packages).await?
    };
    for c in concepts.iter_mut() {
        c.bytes = vec![b'\n'];
        if let Some(package) = &mut c.package {
            package.resources = listings
                .remove(&(c.bundle_id, c.concept_id.clone()))
                .unwrap_or_default()
                .into_iter()
                .map(|r| pgokf_workspace::ResourceFile {
                    concept_id: r.concept_id,
                    class: r.class,
                    path: r.path,
                    sha256: r.sha256,
                    file_hash: String::new(),
                    bytes: vec![b'\n'],
                })
                .collect();
        }
    }
    Ok(())
}

/// Build the tree (sources read through the audited `get_concept_source`)
/// and send it as a zip to unpack at the workspace root.
async fn plugins_zip(State(app): State<Shared>, Query(params): Query<PluginParams>) -> PageResult {
    let (form, chosen, selection, base_model) = params.normalize()?;
    if let Some(problem) = form.problems() {
        return Err(AppError::bad_request(problem));
    }
    if selection.is_empty() {
        return Err(AppError::bad_request(
            "give a query, a bundle, a type, a tag, or concept ids to build a plugin",
        ));
    }
    // Shed rather than queue: a build holds a pooled reader and up to a few
    // hundred audited reads for its whole run, so waiting ones would pile onto
    // the in-flight limit and the connection pool. When the few slots are
    // taken, the caller is told to come back rather than made to wait.
    let _building = app.builds.try_acquire().map_err(|_| {
        AppError::with(
            StatusCode::SERVICE_UNAVAILABLE,
            "The catalog is busy building plugins; try again in a moment.",
        )
    })?;
    let mut client = app.db.checkout().await?;
    let options = build_options(&app, &form, &chosen, base_model);
    let plugin = pgokf_workspace::build(client.client(), &options, &selection)
        .await
        .map_err(workspace_error)?;
    client.finish();
    let bytes = pgokf_workspace::zip(&plugin)?;
    let filename = format!("{}-{}.zip", plugin.name, plugin.target);
    Ok((
        [
            (
                header::CONTENT_TYPE,
                HeaderValue::from_static("application/zip"),
            ),
            (
                header::CONTENT_DISPOSITION,
                HeaderValue::from_str(&format!("attachment; filename=\"{filename}\""))
                    .unwrap_or_else(|_| HeaderValue::from_static("attachment")),
            ),
        ],
        bytes,
    )
        .into_response())
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
    let package = match c.concept_type.as_deref() {
        Some("Skill") => app.db.package(bundle_id, &concept_id).await?,
        _ => None,
    };
    let resource = match c.concept_type.as_deref() {
        Some("Script" | "Reference") => app.db.resource(bundle_id, &concept_id).await?,
        _ => None,
    };
    let mut value = serde_json::to_value(&c).unwrap_or(Value::Null);
    value["links"] = serde_json::json!({ "outgoing": outgoing, "incoming": incoming });
    value["package"] = serde_json::to_value(package).unwrap_or(Value::Null);
    value["resource"] = serde_json::to_value(resource).unwrap_or(Value::Null);
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

    #[test]
    fn a_secret_shown_once_is_never_cached() {
        // Arrange
        let response = Html("pgokf_secret").into_response();

        // Act
        let response = shown_once(response);

        // Assert
        assert_eq!(
            response
                .headers()
                .get(header::CACHE_CONTROL)
                .and_then(|value| value.to_str().ok()),
            Some("no-store")
        );
    }

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
    fn same_origin_reads_fetch_metadata_first_and_origin_second() {
        use axum::http::HeaderName;

        // Arrange
        let with = |pairs: &[(&str, &str)]| {
            let mut h = HeaderMap::new();
            for (k, v) in pairs {
                h.insert(
                    HeaderName::from_bytes(k.as_bytes()).unwrap(),
                    HeaderValue::from_str(v).unwrap(),
                );
            }
            h
        };

        // Act / Assert
        assert!(same_origin(&with(&[("sec-fetch-site", "same-origin")])));
        assert!(same_origin(&with(&[("sec-fetch-site", "none")])));
        assert!(!same_origin(&with(&[
            ("sec-fetch-site", "cross-site"),
            ("origin", "http://x"),
            ("host", "x")
        ])));
        assert!(same_origin(&with(&[
            ("origin", "http://catalog.example:8080"),
            ("host", "catalog.example:8080")
        ])));
        assert!(!same_origin(&with(&[
            ("origin", "http://evil.example"),
            ("host", "catalog.example:8080")
        ])));
        assert!(
            same_origin(&with(&[("host", "catalog.example")])),
            "a non-browser client"
        );
    }

    #[test]
    fn only_the_login_assets_and_health_are_open_to_nobody() {
        // Arrange / Act / Assert
        assert!(open_to_anyone("/login") && open_to_anyone("/logout"));
        assert!(open_to_anyone("/static/app.css") && open_to_anyone("/api/health"));
        assert!(
            !open_to_anyone("/") && !open_to_anyone("/api/search") && !open_to_anyone("/bundles")
        );
    }

    #[test]
    fn workflow_inputs_are_validated_and_next_stays_on_site() {
        // Arrange / Act / Assert
        assert_eq!(
            validated_bundle_name(" team-runbooks ").ok().expect("ok"),
            "team-runbooks"
        );
        assert!(validated_bundle_name(".hidden").is_err());
        assert!(validated_bundle_name("a/b").is_err());
        assert_eq!(
            validated_directory("/runbooks/db/").ok().expect("ok"),
            "runbooks/db"
        );
        assert_eq!(validated_directory("").ok().expect("ok"), "");
        assert!(validated_directory("../up").is_err());
        assert_eq!(
            validated_file_name("C:\\docs\\My Note.md")
                .ok()
                .expect("ok"),
            "My-Note.md"
        );
        assert!(validated_file_name("notes.txt").is_err());
        assert!(validated_file_name(".md").is_err());
        assert_eq!(safe_next("/bundles/3"), "/bundles/3");
        assert_eq!(safe_next("//evil.example"), "/");
        assert_eq!(safe_next("/\\evil.example"), "/");
        assert_eq!(safe_next("/x\r\nSet-Cookie: a=b"), "/");
        assert!(validated_directory(".git/hooks").is_err());
        assert_eq!(
            validated_directory(".github/skills").ok().expect("ok"),
            ".github/skills"
        );
        assert_eq!(safe_next("https://evil.example/"), "/");
        assert_eq!(safe_next(""), "/");
    }

    #[test]
    fn parse_picks_keeps_the_good_lines_and_reports_the_first_bad_one() {
        // Arrange / Act
        let (picks, problem) = parse_picks("3:skills/a/SKILL\n\nnope\n3:skills/a/SKILL\n1:x");
        let (clean, none) = parse_picks("");

        // Assert
        assert_eq!(
            picks.iter().map(ToString::to_string).collect::<Vec<_>>(),
            ["3:skills/a/SKILL", "1:x"]
        );
        assert!(problem.as_deref().is_some_and(|p| p.contains("\"nope\"")));
        assert!(clean.is_empty() && none.is_none());
    }

    #[test]
    fn capped_listings_report_the_extra_row_and_drop_it() {
        // Arrange / Act
        let (full, cut) = capped(vec![1, 2, 3, 4], 3);
        let (short, whole) = capped(vec![1, 2], 3);

        // Assert
        assert_eq!((full, cut), (vec![1, 2, 3], true));
        assert_eq!((short, whole), (vec![1, 2], false));
    }

    #[test]
    fn plugin_params_split_lists_and_build_a_stable_query_string() {
        // Arrange: the everything rule in a bundle, narrowed by tags and ids.
        let params = PluginParams {
            kind: "skills".to_owned(),
            agent: "codex".to_owned(),
            name: "Ops".to_owned(),
            bundle: "2".to_owned(),
            all: "1".to_owned(),
            tags: "a, b".to_owned(),
            ids: "x\ny".to_owned(),
            verified: "on".to_owned(),
            ..PluginParams::default()
        };

        // Act
        let (form, chosen, selection, _) = params.normalize().ok().expect("valid");

        // Assert
        assert_eq!(chosen.target, Target::Codex);
        assert!(selection.all);
        assert_eq!(selection.bundle_ids, vec![2]);
        assert_eq!(selection.tags, vec!["a", "b"]);
        assert_eq!(selection.concept_ids, vec!["x", "y"]);
        assert!(selection.verified_only);
        assert_eq!(
            form.query_string,
            "kind=skills&agent=Codex&name=Ops&bundle=2&all=1&tags=a%2C%20b&ids=x%2C%20y&verified=1"
        );
        let with = PluginParams {
            tools: "1".to_owned(),
            mcp: "on".to_owned(),
            ..PluginParams::default()
        };
        let (form, ..) = with.normalize().ok().expect("valid");
        assert!(form.with_mcp && form.with_tools && !form.with_guide);
        assert!(form.query_string.ends_with("mcp=1&tools=1"));
        assert!(
            PluginParams {
                target: "nope".to_owned(),
                ..PluginParams::default()
            }
            .normalize()
            .is_err()
        );
        assert!(
            PluginParams {
                kind: "plugin".to_owned(),
                ..PluginParams::default()
            }
            .normalize()
            .is_err()
        );
    }

    #[test]
    fn narrowing_fields_wait_until_everything_is_taken() {
        // Arrange: a bundle browsed and types typed, but nothing taken.
        let browsing = PluginParams {
            bundle: "2".to_owned(),
            types: "Runbook".to_owned(),
            picks: "2:runbooks/a".to_owned(),
            ..PluginParams::default()
        };

        // Act
        let (form, _, selection, _) = browsing.normalize().ok().expect("valid");

        // Assert: only the tick counts; the fields are echoed for later.
        assert!(!selection.all && selection.bundle_ids.is_empty() && selection.types.is_empty());
        assert_eq!(selection.picks.len(), 1);
        assert_eq!(form.types, "Runbook");
        assert_eq!(form.bundle, "2");
        assert!(form.query_string.contains("bundle=2&types=Runbook"));
        assert!(!form.query_string.contains("all="));
    }

    #[test]
    fn the_agent_resolves_by_id_label_or_short_label_within_its_kind() {
        // Arrange / Act
        let by_id = resolve_agent("skills", "gemini-cli", "", "")
            .ok()
            .expect("ok");
        let by_label = resolve_agent("skills", "github copilot", "", "")
            .ok()
            .expect("ok");
        let by_short = resolve_agent("prompt-bundle", "Ollama / bare model", "", "")
            .ok()
            .expect("ok");
        let defaulted = resolve_agent("skills", "", "", "").ok().expect("ok");
        let legacy = resolve_agent("", "", "", "cursor").ok().expect("ok");
        let wrong_kind = resolve_agent("agent-plugin", "Codex", "", "")
            .ok()
            .expect("ok");

        // Assert
        assert_eq!(by_id.target, Target::GeminiCli);
        assert_eq!(by_label.target, Target::Copilot);
        assert_eq!(by_label.agent, "GitHub Copilot");
        assert_eq!(by_short.target, Target::Ollama);
        assert_eq!(
            defaulted.target,
            Target::ClaudeCode,
            "the kind's first agent"
        );
        assert_eq!(legacy.target, Target::Cursor);
        assert_eq!(legacy.kind, Shape::Skills);
        assert_eq!(
            wrong_kind.target,
            Target::Custom,
            "a skills agent named under another kind is a new agent of that kind"
        );
        assert!(resolve_agent("", "", "", "nope").is_err());
        assert!(resolve_agent("plugin", "x", "", "").is_err());
    }

    #[test]
    fn an_unknown_agent_is_added_with_a_directory_or_reported() {
        // Arrange / Act
        let added = resolve_agent("skills", " Acme Agent ", "", "")
            .ok()
            .expect("ok");
        let placed = resolve_agent("skills", "Acme", ".acme/skills/", "")
            .ok()
            .expect("ok");
        let unsafe_dir = resolve_agent("skills", "Acme", "../up", "")
            .ok()
            .expect("ok");
        let plugin = resolve_agent("agent-plugin", "Acme", "", "")
            .ok()
            .expect("ok");

        // Assert
        assert_eq!(added.target, Target::Custom);
        assert_eq!(added.agent, "Acme Agent");
        assert_eq!(added.skills_dir, DEFAULT_SKILLS_DIR);
        assert!(added.note().contains(".agents/skills/"));
        assert_eq!(placed.skills_dir, ".acme/skills");
        assert_eq!(
            placed
                .harness
                .as_ref()
                .and_then(|h| h.skills_dir.as_deref()),
            Some(".acme/skills")
        );
        assert!(
            unsafe_dir
                .problem
                .as_deref()
                .is_some_and(|p| p.contains("relative path"))
        );
        assert!(unsafe_dir.harness.is_none());
        assert_eq!(plugin.target, Target::Custom);
        assert_eq!(plugin.kind, Shape::AgentPlugin);
        assert!(plugin.skills_dir.is_empty());
        assert!(plugin.note().contains("standard agent plugin layout"));
        let (form, ..) = PluginParams {
            kind: "skills".to_owned(),
            agent: "Acme".to_owned(),
            skills_dir: "../up".to_owned(),
            picks: "1:a".to_owned(),
            ..PluginParams::default()
        }
        .normalize()
        .ok()
        .expect("valid");
        assert!(form.custom && form.agent_problem.is_some());
        assert!(
            form.query_string
                .starts_with("kind=skills&agent=Acme&skills_dir=..%2Fup")
        );
    }

    #[test]
    fn graph_json_uses_composite_node_ids_and_a_group_legend() {
        // Arrange
        let node =
            |bundle_id: i64, name: &str, title: Option<&str>, degree: i64| crate::db::GraphNode {
                bundle_id,
                bundle_name: name.to_owned(),
                id: "a/b".to_owned(),
                title: title.map(str::to_owned),
                concept_type: Some("Guide".to_owned()),
                path: "a/b.md".to_owned(),
                hops: 0,
                degree,
            };
        let graph = Graph {
            nodes: vec![
                node(2, "docs", None, 1),
                node(1, "sample", Some("Other"), 0),
            ],
            links: vec![crate::db::GraphLink {
                bundle_id: 2,
                source: "a/b".to_owned(),
                target: "a/b".to_owned(),
                count: 1,
                relations: vec![],
                texts: vec![],
            }],
            total: 2,
        };

        // Act
        let value = graph_json(None, 0, &graph, ColorBy::Bundle);

        // Assert
        assert_eq!(value["nodes"][0]["id"], "2:a/b");
        assert_eq!(value["nodes"][1]["id"], "1:a/b");
        assert_eq!(value["nodes"][0]["title"], "a/b");
        assert_eq!(value["nodes"][0]["href"], "/concepts/2/a/b");
        assert_eq!(value["links"][0]["source"], "2:a/b");
        assert_eq!(value["legend"], serde_json::json!(["docs", "sample"]));
        assert_eq!(value["color_by"], "bundle");
    }

    #[test]
    fn catalog_graph_params_parse_the_seed_and_bound_the_limit() {
        // Arrange
        let good = CatalogGraphParams {
            seed: "2:runbooks/a".to_owned(),
            limit: "5000".to_owned(),
            ..CatalogGraphParams::default()
        };
        let bad = CatalogGraphParams {
            seed: "runbooks/a".to_owned(),
            ..CatalogGraphParams::default()
        };

        // Act & Assert
        assert_eq!(
            good.seed().ok().flatten(),
            Some((2, "runbooks/a".to_owned()))
        );
        assert_eq!(good.limit(), GRAPH_DEFAULT_NODES);
        assert!(bad.seed().is_err());
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
        assert_eq!(
            resource_disposition("skills/deploy/scripts/check.sh", false),
            "attachment; filename=\"check.sh\"; filename*=UTF-8''check.sh"
        );
        assert_eq!(
            resource_disposition("skills/deploy/SKILL", true),
            "attachment; filename=\"SKILL.md\"; filename*=UTF-8''SKILL.md"
        );
        assert_eq!(
            resource_disposition("skills/deploy/references/SKILL", false),
            "attachment; filename=\"SKILL\"; filename*=UTF-8''SKILL"
        );
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
