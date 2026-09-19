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
    DefaultBodyLimit, Form, FromRequestParts, Multipart, Path, Query, RawQuery, Request, State,
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
    Cursor, Db, DuplicateGroup, EdgeSource, Facet, Failure, Graph, Hit, Link, Neighbor,
    PackageInfo, PersonalItem, RegistryRepository, ResourceInfo, ReviewItem, SearchQuery,
    StaleConcept, SyncLogEntry, SyncOutcome, Version,
};
use crate::graph::{GraphEdge, GraphNode};
use crate::links::Resolver;
use crate::mcp_tokens::{McpToken, McpTokens, Minted};
use crate::oidc::OidcAuth;
use crate::producer::{CredentialInfo, ProducerError, RegistrationReceipt};
use crate::provider::{ProviderDraft, SecretChange};
use crate::provider_settings::{ProviderKind, ProviderSettings};
use crate::store::DocumentStore;
use crate::type_groups;
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
    /// The repository-registry producer's admin API client, for the
    /// Registry tab's credential controls; `None` when no producer admin
    /// URL/token pair is configured. Holds the static admin bearer token -
    /// server-side only, never rendered.
    pub producer: Option<crate::producer::ProducerAdmin>,
    pub catalog_name: String,
    pub tenant: Option<String>,
    /// The library version seen at startup, for the footer.
    pub version: String,
    /// Test seam: the fixed answer of the registry tenant-visibility probe
    /// ([`Db::registry_repository_visible`]). `None` - always in production -
    /// asks the database.
    #[cfg(test)]
    pub registry_visible: Option<bool>,
    /// Test seam: registry rows served instead of the database read
    /// ([`Db::registry_repositories`]). `None` - always in production -
    /// asks the database.
    #[cfg(test)]
    pub registry_rows: Option<Vec<RegistryRepository>>,
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
        .route(
            "/admin/registry",
            get(admin_registry_page).post(admin_registry),
        )
        .route("/admin/registry/new", get(admin_repository_new_page))
        .route(
            "/admin/registry/{id}",
            get(admin_repository_page).post(admin_repository),
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
    /// Back to the first page when this is a continued one. Keyset cursors
    /// only move forward, so this (plus the browser's Back) is the honest
    /// "previous".
    pub prev_url: Option<String>,
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
            prev_url: None,
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
    #[cfg(test)]
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

/// One `<optgroup>` of the search page's exact-type select: a display
/// group's label and its observed member types with their counts. The
/// grouping is presentational only - every option submits the legacy
/// `type` parameter verbatim.
pub(crate) struct TypeSelectGroup {
    /// The optgroup's label, e.g. "Documents (33)".
    pub label: String,
    pub types: Vec<FacetOption>,
}

/// The search page's two type controls: a group select submitting the
/// separate `type_group` parameter (slugs), and an exact-type select
/// submitting the legacy `type` parameter verbatim. Two parameters keep
/// the namespaces collision-free: no exact type string is ever read as a
/// group.
pub(crate) struct TypeSelects {
    pub groups: Vec<FacetOption>,
    pub types: Vec<TypeSelectGroup>,
}

impl TypeSelects {
    #[cfg(test)]
    fn empty() -> Self {
        Self {
            groups: Vec::new(),
            types: Vec::new(),
        }
    }
}

/// Build the type controls from the observed type facets. Groups come from
/// the live facets (never a closed list: unknown types appear under
/// "Other"), only groups with observed members are listed, and a current
/// choice nothing observed still renders, selected, so it can be cleared.
fn type_selects(observed: &[Facet], selected_group: &str, selected_type: &str) -> TypeSelects {
    let groups = type_groups::groups_from_facets(observed);
    let mut group_options: Vec<FacetOption> = groups
        .iter()
        .map(|g| FacetOption {
            value: g.slug.to_owned(),
            label: format!("{} ({})", g.label, g.count),
            selected: g.slug == selected_group,
        })
        .collect();
    // A selected group with no observed members still shows, selected.
    if !selected_group.is_empty() && !group_options.iter().any(|o| o.selected) {
        group_options.push(FacetOption {
            value: selected_group.to_owned(),
            label: type_groups::label_of(selected_group),
            selected: true,
        });
    }
    let mut types: Vec<TypeSelectGroup> = groups
        .iter()
        .map(|g| TypeSelectGroup {
            label: format!("{} ({})", g.label, g.count),
            types: g
                .types
                .iter()
                .map(|f| FacetOption {
                    selected: f.value == selected_type,
                    label: format!("{} ({})", f.value, f.count),
                    value: f.value.clone(),
                })
                .collect(),
        })
        .collect();
    // As does an exact type nothing observed (facet_options' pattern).
    if !selected_type.is_empty()
        && !types
            .iter()
            .flat_map(|g| g.types.iter())
            .any(|o| o.selected)
    {
        types.push(TypeSelectGroup {
            label: "Selected type".to_owned(),
            types: vec![FacetOption {
                value: selected_type.to_owned(),
                label: selected_type.to_owned(),
                selected: true,
            }],
        });
    }
    TypeSelects {
        groups: group_options,
        types,
    }
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
    /// `warn` or `exclude`, as the form echoes it.
    pub stale_policy: String,
    /// One `bundle_id:concept_id` per line.
    pub seeds: String,
    /// Comma-separated namespaced relationship types.
    pub relation_types: String,
    /// `outbound`, `inbound`, or `both`.
    pub direction: String,
    pub hops: String,
    pub require_closure: bool,
    /// A malformed seeds line, shown with the preview; the download refuses it.
    pub seed_problem: Option<String>,
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
    /// download refuses: an agent that cannot be built for, then a malformed
    /// ticked-files or seeds line.
    fn problems(&self) -> Option<String> {
        self.agent_problem
            .clone()
            .or_else(|| self.pick_problem.clone())
            .or_else(|| self.seed_problem.clone())
    }
}

/// What a selection resolves to, before anything is downloaded.
pub(crate) struct PluginPreview {
    pub description: String,
    pub concepts: Vec<ConceptRecord>,
    pub truncated: bool,
    /// How many concepts the catalog reports as not fresh (kept and
    /// labelled), and how many the `exclude` policy dropped.
    pub stale_count: usize,
    pub excluded_count: usize,
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
    /// The type controls (group select + exact-type select), built from
    /// the type facets.
    type_selects: TypeSelects,
    results: ResultsView,
    semantic_available: bool,
    backend_label: String,
}

#[derive(Template)]
#[template(path = "partials/results.html")]
struct ResultsPartial {
    form: SearchForm,
    facets: FacetsView,
    /// Refreshed out of band with the facets, so the type controls track
    /// the catalog scope of the current filters.
    type_selects: TypeSelects,
    results: ResultsView,
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
    Registry,
    Settings,
}

impl AdminTab {
    const ALL: [AdminTab; 6] = [
        AdminTab::People,
        AdminTab::Providers,
        AdminTab::Tokens,
        AdminTab::Bundles,
        AdminTab::Registry,
        AdminTab::Settings,
    ];

    const fn href(self) -> &'static str {
        match self {
            AdminTab::People => "/admin/people",
            AdminTab::Providers => "/admin/providers",
            AdminTab::Tokens => "/admin/tokens",
            AdminTab::Bundles => "/admin/bundles",
            AdminTab::Registry => "/admin/registry",
            AdminTab::Settings => "/admin/settings",
        }
    }

    const fn label(self) -> &'static str {
        match self {
            AdminTab::People => "People",
            AdminTab::Providers => "Identity providers",
            AdminTab::Tokens => "MCP tokens",
            AdminTab::Bundles => "Bundles",
            AdminTab::Registry => "Registry",
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
            AdminTab::Registry => {
                "The repositories the registry producer service reconciles, read from the \
                 catalog's database. Registering a repository and managing its fetch \
                 credential run through the producer's admin API: it reports a credential's \
                 label, type, and last four characters, and the secret itself is never \
                 stored here or shown."
            }
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
    bundles: Vec<AdminBundleRow>,
    /// Whether the cadence column reflects `pg_cron`'s job table (it could
    /// be read); when not, `cadence_note` says why and the cells show an
    /// unknown marker instead of a form.
    cadence_known: bool,
    cadence_note: Option<String>,
}

#[derive(Template)]
#[template(path = "admin/registry.html")]
// The flags mirror the page's three degradation/permission states one to
// one (the cadence column's `cadence_known` precedent on the bundles tab).
#[allow(clippy::struct_excessive_bools)]
struct AdminRegistryPage {
    shell: Shell,
    admin: AdminShell,
    rows: Vec<RegistryRow>,
    /// Whether the registry table could be read at all; when not,
    /// `registry_note` says why and the page shows no rows.
    registry_known: bool,
    registry_note: Option<String>,
    /// Whether pause/resume and poll-interval writes are possible (a
    /// writer connection is on).
    writable: bool,
    /// Whether the add-repository button is offered and the credential
    /// column reflects the producer's answers (a producer admin API is
    /// configured).
    producer_configured: bool,
    /// Whether the credential column reflects the producer's answers; when
    /// not, `credential_note` says why and the cells show an unknown
    /// marker instead of state or forms.
    credentials_known: bool,
    credential_note: Option<String>,
}

/// One row of the Registry tab's table: a registered repository with its
/// credential state as the producer reports it. The project and remote
/// cells link to the repository's own page (`/admin/registry/{id}`); the
/// table itself carries no credential controls, only the status pill.
pub(crate) struct RegistryRow {
    /// The repository id (a UUID, as text) the forms post back.
    pub id: String,
    pub project: String,
    pub key: String,
    pub branch: String,
    /// The canonical remote URL, or the local-checkout marker.
    pub remote: String,
    pub status: String,
    pub paused: bool,
    pub poll_interval_seconds: i32,
    pub last_indexed: String,
    pub last_published: String,
    pub generation: String,
    pub credential: CredentialCell,
}

/// A repository's credential as the cell shows it: set (label, type, last
/// four characters when the producer reports them - never the secret),
/// anonymous, or unknown.
pub(crate) struct CredentialCell {
    /// `false` when the producer's answer for this row could not be read.
    pub known: bool,
    pub set: bool,
    /// `true` when the producer reports the credential `unusable` (the
    /// stored secret no longer unseals): the cell says so rather than
    /// showing an unexplained gap where the last four would be.
    pub unusable: bool,
    /// The set credential: `label · type · …last4`, or `label · type` when
    /// the producer reports no last four (an unusable credential).
    pub summary: String,
    pub updated_at: String,
}

impl CredentialCell {
    fn unknown() -> Self {
        Self {
            known: false,
            set: false,
            unusable: false,
            summary: String::new(),
            updated_at: String::new(),
        }
    }

    fn anonymous() -> Self {
        Self {
            known: true,
            set: false,
            unusable: false,
            summary: String::new(),
            updated_at: String::new(),
        }
    }

    fn set(info: CredentialInfo) -> Self {
        let summary = match &info.secret_last4 {
            Some(last4) => format!("{} · {} · …{}", info.label, info.kind, last4),
            None => format!("{} · {}", info.label, info.kind),
        };
        Self {
            known: true,
            set: true,
            unusable: info.state == "unusable",
            summary,
            updated_at: info.updated_at,
        }
    }
}

impl RegistryRow {
    fn new(repo: RegistryRepository, credential: CredentialCell) -> Self {
        /// A commit hash for display: its short form, as git prints it.
        fn short(commit: Option<&str>) -> String {
            commit.map_or_else(|| "never".to_owned(), |c| c.chars().take(7).collect())
        }
        let key = repo.key;
        Self {
            id: repo.id,
            project: repo.project,
            key,
            branch: repo.branch,
            remote: repo.remote.unwrap_or_else(|| "local checkout".to_owned()),
            paused: repo.status == "paused",
            status: repo.status,
            poll_interval_seconds: repo.poll_interval_seconds,
            last_indexed: short(repo.last_indexed_commit.as_deref()),
            last_published: short(repo.last_published_commit.as_deref()),
            generation: repo
                .last_published_generation
                .map_or_else(|| "\u{2014}".to_owned(), |g| g.to_string()),
            credential,
        }
    }
}

#[derive(Template)]
#[template(path = "admin/repository.html")]
struct AdminRepositoryPage {
    shell: Shell,
    admin: AdminShell,
    repo: RepositoryDetailView,
    /// Whether the credential controls are offered (a producer admin API
    /// is configured).
    producer_configured: bool,
    /// Whether the credential section reflects the producer's answer; when
    /// not, `credential_note` says why and the section shows an unknown
    /// marker instead of state.
    credentials_known: bool,
    credential_note: Option<String>,
    /// Where the producer stages the worktree, as its operator API reports
    /// it (the database reader grant deliberately withholds the column);
    /// `None` while the producer has not answered.
    checkout_path: Option<String>,
}

#[derive(Template)]
#[template(path = "admin/repository-new.html")]
struct AdminRepositoryNewPage {
    shell: Shell,
    admin: AdminShell,
    /// Whether the registration form is offered (a producer admin API is
    /// configured); when not, the page says so instead of showing the form.
    producer_configured: bool,
}

/// One registered repository as its own page shows it: the full registry
/// row (commit hashes unshortened, unlike the table) plus its credential
/// state as the producer reports it.
pub(crate) struct RepositoryDetailView {
    /// The repository id (a UUID, as text) the credential forms post to.
    pub id: String,
    pub project: String,
    pub key: String,
    pub branch: String,
    /// The canonical remote URL, or the local-checkout marker.
    pub remote: String,
    pub status: String,
    pub paused: bool,
    pub poll_interval_seconds: i32,
    pub last_indexed: String,
    pub last_published: String,
    pub generation: String,
    pub credential: CredentialCell,
}

impl RepositoryDetailView {
    fn new(repo: RegistryRepository, credential: CredentialCell) -> Self {
        Self {
            id: repo.id,
            project: repo.project,
            key: repo.key,
            branch: repo.branch,
            remote: repo.remote.unwrap_or_else(|| "local checkout".to_owned()),
            paused: repo.status == "paused",
            status: repo.status,
            poll_interval_seconds: repo.poll_interval_seconds,
            last_indexed: repo
                .last_indexed_commit
                .unwrap_or_else(|| "never".to_owned()),
            last_published: repo
                .last_published_commit
                .unwrap_or_else(|| "never".to_owned()),
            generation: repo
                .last_published_generation
                .map_or_else(|| "\u{2014}".to_owned(), |g| g.to_string()),
            credential,
        }
    }
}

/// One row of the admin bundles table: the bundle and its refresh cadence.
pub(crate) struct AdminBundleRow {
    pub bundle: AdminBundle,
    pub cadence: CadenceView,
}

/// A bundle's refresh cadence as the admin table shows and edits it.
pub(crate) struct CadenceView {
    /// The current cadence, humanized for a preset ("every 15 minutes"),
    /// the raw schedule text for anything else, "Off" when no job exists.
    pub label: String,
    /// The select option matching the current state
    /// (`off`/`15min`/`hourly`/`6h`/`daily`/`custom`).
    pub preset: String,
    /// The current raw schedule, prefilling the custom field.
    pub custom: String,
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
    /// The selected bundle ids, echoed into the sidebar's multi-select.
    bundle_ids: Vec<i64>,
    /// The type-group (and any selected exact-type) checkboxes.
    type_filters: Vec<GraphTypeFilter>,
    limit_options: Vec<(i32, bool)>,
    /// The edge-source select as `(value, selected, label)`.
    edge_options: Vec<(&'static str, bool, &'static str)>,
    /// The JSON endpoint the client draws (the client appends `hops`).
    graph_url: String,
    /// The whole-catalog URL a seeded neighborhood links back to.
    catalog_url: String,
    hops: i32,
    seed_label: Option<String>,
}

/// One checkbox of the graph page's type filter: a type group (submitted
/// as `type_group=<slug>`) or a selected exact type (submitted as the
/// legacy `type` parameter, verbatim).
pub(crate) struct GraphTypeFilter {
    pub value: String,
    pub label: String,
    pub selected: bool,
    /// `true` for a group slug, `false` for an exact type string.
    pub group: bool,
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
    /// Always an exact type string, matched verbatim (the legacy form).
    #[serde(default, rename = "type")]
    concept_type: String,
    /// A type-group slug (`code`, `documents`, `other`) in its own
    /// parameter, so no exact type string is ever reinterpreted.
    #[serde(default)]
    type_group: String,
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
}

/// The normalized form state echoed back into the filters.
#[derive(Debug, Clone)]
pub(crate) struct SearchForm {
    pub q: String,
    pub bundle: String,
    pub concept_type: String,
    pub type_group: String,
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
            ("type_group", &self.type_group),
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
        // The `type` parameter is always an exact type string, matched
        // verbatim (its legacy form); a group arrives in its own
        // `type_group` parameter and expands to observed member types
        // later, once the catalog can be read. The two are mutually
        // exclusive: accepting both would silently drop one.
        let concept_types: Vec<String> = non_empty(&self.concept_type).into_iter().collect();
        let type_group = match non_empty(&self.type_group) {
            None => None,
            Some(slug) if type_groups::is_known_slug(&slug) => Some(slug),
            Some(slug) => {
                return Err(AppError::bad_request(format!("unknown type group {slug}")));
            }
        };
        if type_group.is_some() && !concept_types.is_empty() {
            return Err(AppError::bad_request(
                "type and type_group are mutually exclusive",
            ));
        }
        let form = SearchForm {
            q: self.q.trim().to_owned(),
            bundle: bundle_id.map(|b| b.to_string()).unwrap_or_default(),
            concept_type: self.concept_type.trim().to_owned(),
            type_group: type_group.clone().unwrap_or_default(),
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
            concept_types,
            type_group,
            tags,
            status: non_empty(&form.status),
            trust_tier: non_empty(&form.trust),
            limit,
            after,
        };
        Ok((form, query))
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

/// Expand the search's selected type group into the exact types the data
/// layer filters by. The expansion reads the catalog's complete visible
/// type inventory (in the chosen bundle's scope), so it matches what the
/// grouped select offered and no type beyond the display facets' cap is
/// lost; a group with no observed members leaves `concept_types` empty
/// with `type_group` set, which the data layer reads as "matches nothing",
/// never as "no filter".
async fn resolve_type_group(app: &App, query: &mut SearchQuery) -> Result<(), AppError> {
    let Some(slug) = query.type_group.clone() else {
        return Ok(());
    };
    let observed = app.db.catalog_types(query.bundle_id).await?;
    query.concept_types = type_groups::expand_group(&slug, &observed).unwrap_or_default();
    Ok(())
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
            .map(|c| search_url("/search/results", form, Some(c))),
        prev_url: query
            .after
            .is_some()
            .then(|| search_url("/search", form, None)),
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
    if !form.type_group.is_empty() {
        what.push(format!(
            "type group {}",
            type_groups::label_of(&form.type_group)
        ));
    }
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
            .map(|c| search_url("/search/results", form, Some(c))),
        prev_url: after.is_some().then(|| search_url("/search", form, None)),
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
    let (form, mut query) = params.normalize()?;
    resolve_type_group(&app, &mut query).await?;
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
    let type_selects = type_selects(&facets.types, &form.type_group, &form.concept_type);
    let mut shell = Shell::new(&app, &session, "Search", "search");
    shell.query.clone_from(&form.q);
    shell.filters = form.filter_pairs();
    html(&SearchPage {
        shell,
        form,
        bundles,
        facets,
        type_selects,
        results,
        semantic_available,
        backend_label,
    })
}

/// The htmx partial: the result list (with its heading and the refreshed
/// facets swapped out of band). The pushed URL is the full search URL —
/// cursor included — so reload, back, and share keep every filter and the
/// page they were on.
async fn search_results(
    State(app): State<Shared>,
    Query(params): Query<SearchParams>,
) -> PageResult {
    let (form, mut query) = params.normalize()?;
    resolve_type_group(&app, &mut query).await?;
    let names = bundle_names(&app).await?;
    let results = run_search(&app, &form, &query, &names).await?;
    let facets = load_facets(&app, &form, &query, &names).await?;
    let type_selects = type_selects(&facets.types, &form.type_group, &form.concept_type);
    let push_url = search_url("/search", &form, query.after.as_ref());
    let mut response = html(&ResultsPartial {
        form,
        facets,
        type_selects,
        results,
        oob: true,
    })?;
    if let Ok(value) = HeaderValue::from_str(&push_url) {
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

/// The URL a node's "Explore" actions load: the source-aware seeded form of
/// the catalog graph endpoint, so exploring from a filtered picture keeps
/// that picture's edge source instead of resetting to the concept
/// endpoint's both-sources default.
fn explore_href(bundle_id: i64, concept_id: &str, edges: EdgeSource) -> String {
    format!(
        "/api/graph?seed={}&edges={}",
        filters::percent_encode(&format!("{bundle_id}:{concept_id}")),
        edges.as_str()
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
/// unique across bundles; nodes carry their page and explore URLs so the
/// client never builds URLs from ids, and the explore URL carries the
/// picture's own edge source. Edges of every selected source share the
/// `links` array, each labelled by `kind` (`link` or `relationship`) so the
/// client styles and toggles them per source; typed edges carry their
/// relation types, each with its own direction, and whether every folded
/// row is undirected (the client draws an arrow unless they are).
fn graph_json(
    seed: Option<(i64, &str)>,
    hops: i32,
    graph: &Graph,
    color_by: ColorBy,
    edges: EdgeSource,
) -> Value {
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
                "graph_href": explore_href(n.bundle_id, &n.id, edges),
            })
        })
        .collect();
    let links: Vec<Value> = graph
        .links
        .iter()
        .map(|l| {
            serde_json::json!({
                "kind": "link",
                "source": node_id(l.bundle_id, &l.source),
                "target": node_id(l.bundle_id, &l.target),
                "count": l.count,
                "relations": l.relations,
                "texts": l.texts,
                "undirected": false,
            })
        })
        .chain(graph.relationships.iter().map(|r| {
            serde_json::json!({
                "kind": "relationship",
                "source": node_id(r.source_bundle_id, &r.source),
                "target": node_id(r.target_bundle_id, &r.target),
                "count": r.count,
                "relations": r.relations,
                "texts": Vec::<String>::new(),
                "undirected": r.undirected,
            })
        }))
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
    let across_writers = match store.content_name() {
        Some(name) => Some(writer.lock_content_bundle(name).await?),
        None => None,
    };
    let outcome = store.apply(writer, changes, removals).await;
    if let Some(lock) = across_writers {
        lock.release().await;
    }
    Ok(outcome?)
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
    // A change addresses a content bundle by name, and the catalog resolves
    // that name within this instance's own tenant. If it does not resolve to
    // the bundle being changed, the change would land on another row, so it
    // is refused here - before anything is read or written - and said in
    // words rather than left to the backstop in the store.
    if let Some(name) = store.content_name()
        && !writer.content_name_is_only(name, store.bundle_id()).await?
    {
        return Err(AppError::bad_request(format!(
            "This site cannot change {name}: a change addresses a content bundle by name, and \
             in this site's own tenant that name is not this bundle alone. A bundle belonging \
             to another tenant is changed from a site serving that tenant (OKF_TENANT)."
        )));
    }
    // Rebuilding sends back what the catalog stores. A bundle carrying
    // anything it does not store the bytes of would come back without it,
    // so the change is refused rather than made at that cost.
    if let Some(carries) = writer.bundle_carries_unstored(store.bundle_id()).await? {
        return Err(AppError::bad_request(format!(
            "This bundle carries {carries}. Those are the bundle's own bookkeeping, not \
             documents, so the catalog keeps no bytes of them - and changing one document \
             rewrites the whole bundle from what it does keep, which would drop them. The \
             change is refused rather than made at that cost. A bundle streamed in by an \
             ingestion companion is changed at its source and re-synced; one built here can \
             be re-created without those files."
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

/// Create a content bundle from the documents uploaded into it.
///
/// Held against every other writer of that name for the whole check-and-
/// create: `register_bundle_content` is a full snapshot resync, so a name
/// created underneath this - by `pgokf-mcp`, or a second instance of this UI
/// - would be resynced down to only the files uploaded here.
async fn create_bundle_with(
    app: &App,
    access: &Access<'_>,
    name: &str,
    documents: Vec<BundleFile>,
) -> Result<SyncOutcome, AppError> {
    let _one_at_a_time = app.rebuilds.lock().await;
    let across_writers = access.writer.lock_content_bundle(name).await?;
    let registered = match app.db.content_bundle_exists(name).await {
        Ok(false) => access
            .writer
            .register_content(name, &documents)
            .await
            .map_err(AppError::from),
        Ok(true) => Err(AppError::bad_request(format!(
            "A bundle called {name} already exists. Choose it in the list to add to it; \
             creating it again would replace everything already in it."
        ))),
        Err(error) => Err(AppError::from(error)),
    };
    across_writers.release().await;
    registered
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
        UploadTarget::New(name) => create_bundle_with(app, access, name, documents).await?,
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

/// The refresh cadence select's scheduled choices: `(value, label, cron)`.
/// `off` and `custom` are handled by the form logic and carry no schedule.
const CADENCE_PRESETS: &[(&str, &str, &str)] = &[
    ("15min", "every 15 minutes", "*/15 * * * *"),
    ("hourly", "hourly", "0 * * * *"),
    ("6h", "every 6 hours", "0 */6 * * *"),
    ("daily", "daily", "0 3 * * *"),
];

/// The longest accepted schedule text, mirroring the extension's bound
/// (`crates/extension/src/catalog/schedule.rs`), so a value that would be
/// refused there never reaches the database.
const MAX_SCHEDULE_LEN: usize = 128;

/// How one scheduled refresh reads on the admin page. A preset's own cron
/// expression humanizes to the preset's label; anything else (set by hand
/// or by another client) shows its raw schedule text, and no job is "Off".
fn cadence_view(schedule: Option<&str>) -> CadenceView {
    let Some(schedule) = schedule else {
        return CadenceView {
            label: "Off".to_owned(),
            preset: "off".to_owned(),
            custom: String::new(),
        };
    };
    let trimmed = schedule.trim();
    match CADENCE_PRESETS.iter().find(|(_, _, cron)| cron == &trimmed) {
        Some((value, label, _)) => CadenceView {
            label: (*label).to_owned(),
            preset: (*value).to_owned(),
            custom: trimmed.to_owned(),
        },
        None => CadenceView {
            label: trimmed.to_owned(),
            preset: "custom".to_owned(),
            custom: trimmed.to_owned(),
        },
    }
}

/// The schedule a cadence form submission asks for: `None` for `off` (the
/// job goes away), the preset's cron expression, or the validated custom
/// text. An unknown select value or a malformed custom schedule is refused
/// here, before any statement runs.
fn resolve_cadence(cadence: &str, custom: &str) -> Result<Option<String>, AppError> {
    match cadence.trim() {
        "" | "off" => Ok(None),
        "custom" => validate_cron_schedule(custom).map(Some),
        value => CADENCE_PRESETS
            .iter()
            .find(|(v, _, _)| *v == value)
            .map(|(_, _, cron)| Some((*cron).to_owned()))
            .ok_or_else(|| AppError::bad_request("Choose a refresh cadence from the list.")),
    }
}

/// Validate the custom schedule of the cadence form and normalize it to
/// what is sent to `pg_cron`: non-empty, within the extension's length
/// bound, NUL-free, and either a 5-field cron expression or an explicitly
/// supported interval phrase. Supported phrases translate BEFORE they
/// reach the scheduler: `1-59 seconds` passes through as `pg_cron`'s own
/// interval syntax (the only interval its parser accepts), while minute
/// and hour phrases become the equivalent 5-field cron expression
/// (`30 minutes` -> `*/30 * * * *`, `1 hour` -> `0 * * * *`). Anything
/// `pg_cron` could not schedule as entered (`60 seconds`, `90 minutes`) is
/// refused here; this is the pre-flight screen, and a schedule that passes
/// here but not `pg_cron`'s parser comes back from the database as a form
/// error the same way.
fn validate_cron_schedule(raw: &str) -> Result<String, AppError> {
    let schedule = raw.trim();
    let bad = |why: String| {
        AppError::bad_request(format!(
            "The custom schedule is not one pg_cron accepts: {why}. Give five cron fields \
             (e.g. */30 * * * *) or an interval: 1-59 seconds, 1-59 minutes, or a number of \
             hours that divides the day."
        ))
    };
    if schedule.is_empty() {
        return Err(bad("it is empty".to_owned()));
    }
    if schedule.len() > MAX_SCHEDULE_LEN {
        return Err(bad(format!("it is longer than {MAX_SCHEDULE_LEN} bytes")));
    }
    if schedule.contains('\0') {
        return Err(bad("it contains a NUL byte".to_owned()));
    }
    match interval_phrase(schedule) {
        Some(Ok(translated)) => Ok(translated),
        Some(Err(why)) => Err(bad(why)),
        None if valid_cron_expression(schedule) => Ok(schedule.to_owned()),
        None => Err(bad(
            "give five fields (minute hour day-of-month month day-of-week)".to_owned(),
        )),
    }
}

/// A `<count> <unit>` interval phrase, when the input has that shape.
/// `Some(Ok(_))` is the schedule to hand to `pg_cron`: a 1-59 second
/// phrase passes through as `pg_cron`'s own interval syntax; a minute
/// phrase (1-59) becomes `*/n * * * *`; an hour phrase that divides the
/// day (1, 2, 3, 4, 6, 8, 12, 24) becomes `0 */n * * *` (with `1 hour` ->
/// `0 * * * *` and `24 hours` -> `0 0 * * *`). `Some(Err(_))` refuses a
/// phrase-shaped input the scheduler could not honor as entered - above
/// all `60 seconds`, which `pg_cron`'s interval parser (1-59 seconds only)
/// rejects. `None` means the input is not phrase-shaped at all and the
/// cron parser decides.
fn interval_phrase(schedule: &str) -> Option<Result<String, String>> {
    let words: Vec<&str> = schedule.split_whitespace().collect();
    let [count, unit] = words.as_slice() else {
        return None;
    };
    let unit = unit.to_ascii_lowercase();
    let kind = match unit.as_str() {
        "second" | "seconds" => "second",
        "minute" | "minutes" => "minute",
        "hour" | "hours" => "hour",
        _ => return None,
    };
    let Ok(n) = count.parse::<u64>() else {
        return Some(Err(format!("{count:?} is not a count")));
    };
    if n == 0 {
        return Some(Err("the count must be positive".to_owned()));
    }
    Some(match kind {
        "second" if n <= 59 => Ok(format!("{n} seconds")),
        "second" => Err(format!(
            "pg_cron's interval syntax accepts only 1-59 seconds, not {n}"
        )),
        "minute" if n <= 59 => Ok(format!("*/{n} * * * *")),
        "minute" => Err(format!(
            "{n} minutes does not map onto a minute field; give five cron fields"
        )),
        "hour" if n == 1 => Ok("0 * * * *".to_owned()),
        "hour" if n == 24 => Ok("0 0 * * *".to_owned()),
        "hour" if 24 % n == 0 => Ok(format!("0 */{n} * * *")),
        "hour" => Err(format!(
            "{n} hours does not divide the day (1, 2, 3, 4, 6, 8, 12, or 24 do); \
             give five cron fields"
        )),
        _ => unreachable!("unit is one of second, minute, hour"),
    })
}

/// A 5-field cron expression; every field is a list of atoms or ranges with
/// optional steps, the atoms numeric within their field's range or a month
/// or weekday name.
fn valid_cron_expression(schedule: &str) -> bool {
    const MONTHS: [&str; 12] = [
        "JAN", "FEB", "MAR", "APR", "MAY", "JUN", "JUL", "AUG", "SEP", "OCT", "NOV", "DEC",
    ];
    const WEEKDAYS: [&str; 7] = ["SUN", "MON", "TUE", "WED", "THU", "FRI", "SAT"];
    let fields: Vec<&str> = schedule.split_whitespace().collect();
    let [minute, hour, day, month, weekday] = fields.as_slice() else {
        return false;
    };
    valid_cron_field(minute, 0, 59, &[])
        && valid_cron_field(hour, 0, 23, &[])
        && valid_cron_field(day, 1, 31, &[])
        && valid_cron_field(month, 1, 12, &MONTHS)
        && valid_cron_field(weekday, 0, 7, &WEEKDAYS)
}

/// One cron field: a comma list of `atom`, `atom-atom`, or either with a
/// `/step` (a positive integer); `*` stands for the whole range.
fn valid_cron_field(field: &str, min: u32, max: u32, names: &[&str]) -> bool {
    let atom = |token: &str| {
        if let Ok(value) = token.parse::<u32>() {
            return (min..=max).contains(&value);
        }
        names.iter().any(|name| token.eq_ignore_ascii_case(name))
    };
    field.split(',').all(|item| {
        let (base, step) = item
            .split_once('/')
            .map_or((item, None), |(b, s)| (b, Some(s)));
        let step_ok = step.is_none_or(|s| s.parse::<u32>().is_ok_and(|n| n > 0));
        let base_ok = match base.split_once('-') {
            Some((from, to)) => atom(from) && atom(to),
            None => base == "*" || atom(base),
        };
        step_ok && base_ok
    })
}

/// Whether a catalog failure carries this `SQLSTATE` (used to tell "no
/// `pg_cron` here", "the grant or the extension predates the read surface",
/// and a skewed install apart from a real fault on the schedule read).
fn is_sql_state(error: &anyhow::Error, code: &str) -> bool {
    crate::db::sql_state(error)
        .as_ref()
        .is_some_and(|state| state.code() == code)
}

/// The Bundles tab, read from the catalog. The cadence column reads
/// schedules through the extension's `pgokf.list_scheduled_refreshes`
/// surface (over the writer connection), never `cron.job` directly:
/// `pg_cron` grants `SELECT` on `cron.job` to `PUBLIC` but restricts rows
/// to `username = current_user`, and `pgokf.schedule_refresh` registers
/// every job under the extension owner's identity, so this app's login
/// would read zero rows whatever its grants. When the read is impossible -
/// no writer connection, no `pg_cron` in this database, a skewed extension
/// version, or the grant missing - the column degrades to an unknown
/// marker with a note rather than failing the page.
async fn render_admin_bundles(
    app: &App,
    session: &Session,
    outcome: AdminOutcome,
) -> Result<AdminBundlesPage, AppError> {
    let bundles = app.db.admin_bundles().await?;
    let (schedules, cadence_note) = match &app.writer {
        Some(writer) => match writer.refresh_schedules().await {
            Ok(schedules) => (Some(schedules), None),
            // The extension's read surface raises its curated 22023 when
            // pg_cron is absent (exactly like schedule_refresh); 42P01
            // covers the skewed case where the cron.job relation itself is
            // gone. Either way: not a page failure, an explained unknown.
            Err(error) if is_sql_state(&error, "22023") || is_sql_state(&error, "42P01") => (
                None,
                Some(
                    "pg_cron is not installed in this database, so no refresh cadence can be \
                     read or set here (scheduled refresh needs pg_cron; see docs/operations.md)."
                        .to_owned(),
                ),
            ),
            Err(error) if is_sql_state(&error, "42501") => (
                None,
                Some(
                    "The writer connection cannot execute pgokf.list_scheduled_refreshes, so \
                     the cadence column is unknown: the extension grants it to pgokf_reader \
                     (which the app's roles inherit), and a database admin can re-grant \
                     EXECUTE after a partial restore. Setting a cadence additionally needs \
                     the writer role to hold the pgokf_admin tier (pgokf.schedule_refresh is \
                     admin-only)."
                        .to_owned(),
                ),
            ),
            // The read surface was added after the extension this database
            // has installed: an explained unknown, never an Off.
            Err(error) if is_sql_state(&error, "42883") => (
                None,
                Some(
                    "The installed pgokf extension predates the schedule read surface \
                     (pgokf.list_scheduled_refreshes), so the cadence column is unknown; \
                     upgrade the extension to read and set refresh cadences here."
                        .to_owned(),
                ),
            ),
            // A busy pool or a timed-out read must not take the page down
            // with it: the column reads as unknown until a reload answers.
            Err(error)
                if matches!(
                    crate::db::classify(&error),
                    Failure::Busy | Failure::Timeout
                ) =>
            {
                (
                    None,
                    Some(
                        "The schedule read did not answer (the catalog is busy); the cadence \
                         column is unknown for now - reload to try again."
                            .to_owned(),
                    ),
                )
            }
            Err(error) => return Err(AppError::from(error)),
        },
        None => (
            None,
            Some(
                "Refresh cadences need the writer connection (OKF_PG_WRITER_URL); this server \
                 is read-only."
                    .to_owned(),
            ),
        ),
    };
    let by_bundle: HashMap<i64, String> = schedules
        .unwrap_or_default()
        .into_iter()
        .map(|s| (s.bundle_id, s.schedule))
        .collect();
    let known = cadence_note.is_none();
    Ok(AdminBundlesPage {
        shell: Shell::new(app, session, &AdminTab::Bundles.title(), "admin"),
        admin: AdminTab::Bundles.shell(outcome),
        bundles: bundles
            .into_iter()
            .map(|bundle| {
                let cadence = cadence_view(by_bundle.get(&bundle.id).map(String::as_str));
                AdminBundleRow { bundle, cadence }
            })
            .collect(),
        cadence_known: known,
        cadence_note,
    })
}

async fn admin_bundles_page(
    State(app): State<Shared>,
    session: Session,
    Query(params): Query<NoticeParams>,
) -> PageResult {
    admin(&session)?;
    let outcome = AdminOutcome {
        notice: non_empty(&params.notice),
        error: None,
    };
    html(&render_admin_bundles(&app, &session, outcome).await?)
}

/// The Bundles tab again, saying what was wrong with the last action, as a
/// 400 - on the page the action came from.
async fn bundles_refused(app: &App, session: &Session, error: String) -> PageResult {
    let outcome = AdminOutcome {
        notice: None,
        error: Some(error),
    };
    as_refusal(&render_admin_bundles(app, session, outcome).await?)
}

/// What a cadence change produced: a notice, or a refusal the page shows
/// the operator (a form error, not a page fault).
enum CadenceOutcome {
    Done(String),
    Refused(String),
}

/// Schedule (or, for `off`, remove) a bundle's recurring content refresh.
/// This schedules a content refresh only - the job re-reads the bundle's
/// source on a cadence; the freshness state stays the producer's to attest,
/// and nothing here certifies it.
async fn change_cadence(
    writer: &Db,
    bundle_id: i64,
    cadence: &str,
    custom: &str,
) -> Result<CadenceOutcome, AppError> {
    let schedule = match resolve_cadence(cadence, custom) {
        Ok(schedule) => schedule,
        Err(error) => return Ok(CadenceOutcome::Refused(error.message().to_owned())),
    };
    let result = match &schedule {
        Some(schedule) => writer
            .schedule_refresh(bundle_id, schedule)
            .await
            .map(|job| {
                format!(
                    "Bundle {bundle_id} now refreshes {} (job {job}). A refresh re-reads the \
                     source; it does not attest freshness.",
                    cadence_view(Some(schedule)).label
                )
            }),
        None => writer.unschedule_refresh(bundle_id).await.map(|removed| {
            if removed {
                format!(
                    "Bundle {bundle_id} no longer refreshes on a schedule; the job was removed."
                )
            } else {
                format!("Bundle {bundle_id} had no scheduled refresh.")
            }
        }),
    };
    match result {
        Ok(notice) => Ok(CadenceOutcome::Done(notice)),
        // The database's own verdict on the schedule (pg_cron's parser) or
        // the bundle id is a form error, not a page fault.
        Err(error) if crate::db::classify(&error) == Failure::InvalidInput => {
            Ok(CadenceOutcome::Refused(
                crate::db::db_message(&error)
                    .unwrap_or_else(|| "The catalog rejected the cadence.".to_owned()),
            ))
        }
        Err(error) if is_sql_state(&error, "42501") => Ok(CadenceOutcome::Refused(
            "Scheduling a refresh needs the writer connection to hold the pgokf_admin tier: \
             pgokf.schedule_refresh is admin-only."
                .to_owned(),
        )),
        Err(error) => Err(AppError::from(error)),
    }
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
    /// The refresh cadence select (`off`, a preset, or `custom`).
    #[serde(default)]
    cadence: String,
    /// The custom cron schedule, used when `cadence` is `custom`.
    #[serde(default)]
    custom_cron: String,
}

/// Refresh one bundle. A filesystem or object-store bundle is re-read from
/// its source (`pgokf.refresh_bundle`). A content bundle has no source to
/// re-read, so it is rebuilt from the sources the catalog keeps: the same
/// full-snapshot resync the document workflow runs after a change
/// (`apply_change` with an empty change, which re-reads every stored file
/// and calls `register_bundle_content`), under the same concurrency
/// contract - the process-wide one-rebuild-at-a-time lock and the content
/// bundle's cross-writer advisory lock. The rebuild needs `store_source`
/// on; when it is off the action answers with the workflow's readable
/// refusal, which names the admin step.
async fn refresh_one_bundle(app: &App, writer: &Db, id: i64) -> Result<SyncOutcome, AppError> {
    if matches!(app.db.bundle_source(id).await?, Some((kind, _)) if kind == "content") {
        let store = open_store(app, id).await?.map_err(AppError::bad_request)?;
        ensure_store_sources(&store, writer).await?;
        return apply_change(app, writer, &store, Vec::new(), &[]).await;
    }
    let _one_at_a_time = app.rebuilds.lock().await;
    Ok(writer.refresh_bundle(id).await?)
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
            let outcome = refresh_one_bundle(&app, access.writer, id()?).await?;
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
        "cadence" => {
            match change_cadence(access.writer, id()?, &form.cadence, &form.custom_cron).await? {
                CadenceOutcome::Done(notice) => notice,
                CadenceOutcome::Refused(why) => {
                    return bundles_refused(&app, &session, why).await;
                }
            }
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

// ---- Registry -------------------------------------------------------------

/// The registry listing the Registry tab renders. Production always reads
/// the database; tests may pin rows through the [`App`] seam.
async fn registry_repositories(app: &App) -> anyhow::Result<Vec<RegistryRepository>> {
    #[cfg(test)]
    if let Some(rows) = &app.registry_rows {
        return Ok(rows.clone());
    }
    app.db.registry_repositories().await
}

/// Whether the id names a registry row this server's tenant may act on.
/// Production always asks the database; tests may pin the answer through
/// the [`App`] seam.
async fn registry_repository_visible(app: &App, id: &str) -> Result<bool, AppError> {
    #[cfg(test)]
    if let Some(visible) = app.registry_visible {
        return Ok(visible);
    }
    Ok(app.db.registry_repository_visible(id).await?)
}

/// The Registry tab, read from the catalog. The registry itself comes from
/// the external producer service's table through the extension's narrow
/// reader grant; the credential column comes from the producer's admin API
/// (which never returns a secret). Either side degrades to an explained
/// "unknown" rather than failing the page: a database without the producer
/// schema, a missing grant, a busy pool, or a producer that does not answer.
async fn render_admin_registry(
    app: &App,
    session: &Session,
    outcome: AdminOutcome,
) -> Result<AdminRegistryPage, AppError> {
    let (repos, registry_note) = match registry_repositories(app).await {
        Ok(repos) => (repos, None),
        // The producer service does not share this database (or a partial
        // install lost the table): an explained empty page, not a fault.
        Err(error) if is_sql_state(&error, "42P01") => (
            Vec::new(),
            Some(
                "This database holds no repository registry (the producer service's \
                 ast_graph.repository_registry is not present), so there is nothing to \
                 configure here."
                    .to_owned(),
            ),
        ),
        // The narrow grant is missing: the extension grants the listable
        // columns to pgokf_reader where the producer's table exists, and a
        // database admin can re-grant after a partial restore.
        Err(error) if is_sql_state(&error, "42501") => (
            Vec::new(),
            Some(
                "The reader connection cannot see the registry: the extension grants its \
                 listable columns to pgokf_reader (with USAGE on the ast_graph schema) where \
                 the producer's table exists; a database admin can re-grant SELECT after a \
                 partial restore."
                    .to_owned(),
            ),
        ),
        // A busy pool or a timed-out read must not take the page down with
        // it: reload answers later.
        Err(error)
            if matches!(
                crate::db::classify(&error),
                Failure::Busy | Failure::Timeout
            ) =>
        {
            (
                Vec::new(),
                Some(
                    "The registry read did not answer (the catalog is busy); reload to try again."
                        .to_owned(),
                ),
            )
        }
        Err(error) => return Err(AppError::from(error)),
    };
    let registry_known = registry_note.is_none();
    let producer_configured = app.producer.is_some();
    let mut credentials_known = true;
    let mut credential_note = None;
    let mut rows = Vec::with_capacity(repos.len());
    for repo in repos {
        let cell = match app.producer.as_ref() {
            // Once one read finds the producer down, the rest would too:
            // stop asking and mark every remaining cell unknown.
            _ if !credentials_known => CredentialCell::unknown(),
            Some(producer) => match producer.credential(&repo.id).await {
                Ok(Some(info)) => CredentialCell::set(info),
                Ok(None) => CredentialCell::anonymous(),
                Err(ProducerError::Unavailable) => {
                    credentials_known = false;
                    credential_note = Some(
                        "The producer admin API did not answer, so the credential column is \
                         unknown - reload to try again. The registry itself (read from the \
                         database) is unaffected."
                            .to_owned(),
                    );
                    CredentialCell::unknown()
                }
                // A refusal on one repository's read marks that one cell.
                Err(_) => CredentialCell::unknown(),
            },
            None => CredentialCell::unknown(),
        };
        rows.push(RegistryRow::new(repo, cell));
    }
    if !producer_configured && registry_known {
        credential_note = Some(
            "Credential controls need the producer admin API (OKF_PRODUCER_ADMIN_URL and \
             OKF_PRODUCER_ADMIN_TOKEN together); without them a repository's credential state \
             reads as unknown here."
                .to_owned(),
        );
    }
    Ok(AdminRegistryPage {
        shell: Shell::new(app, session, &AdminTab::Registry.title(), "admin"),
        admin: AdminTab::Registry.shell(outcome),
        rows,
        registry_known,
        registry_note,
        writable: app.writer.is_some(),
        producer_configured,
        credentials_known: credentials_known && producer_configured,
        credential_note,
    })
}

async fn admin_registry_page(
    State(app): State<Shared>,
    session: Session,
    Query(params): Query<NoticeParams>,
) -> PageResult {
    admin(&session)?;
    let outcome = AdminOutcome {
        notice: non_empty(&params.notice),
        error: None,
    };
    html(&render_admin_registry(&app, &session, outcome).await?)
}

/// The add-repository page: the registration form on a page of its own,
/// the same pattern the Providers tab uses for its own long form (a
/// panel-head button leading to a dedicated `/new` page).
fn render_admin_repository_new(
    app: &App,
    session: &Session,
    outcome: AdminOutcome,
) -> AdminRepositoryNewPage {
    AdminRepositoryNewPage {
        shell: Shell::new(app, session, "Administration · Add a repository", "admin"),
        admin: AdminTab::Registry.shell(outcome),
        producer_configured: app.producer.is_some(),
    }
}

async fn admin_repository_new_page(State(app): State<Shared>, session: Session) -> PageResult {
    admin(&session)?;
    html(&render_admin_repository_new(
        &app,
        &session,
        AdminOutcome::default(),
    ))
}

/// The add-repository page again, saying what was wrong with the
/// registration: a 400 for a refusal of what was asked, a 503 when the
/// producer admin API did not answer - on the page the form is on, never a
/// bare error.
fn repository_new_refused(
    app: &App,
    session: &Session,
    error: String,
    unavailable: bool,
) -> PageResult {
    let outcome = AdminOutcome {
        notice: None,
        error: Some(error),
    };
    let mut response = html(&render_admin_repository_new(app, session, outcome))?;
    *response.status_mut() = if unavailable {
        StatusCode::SERVICE_UNAVAILABLE
    } else {
        StatusCode::BAD_REQUEST
    };
    Ok(response)
}

/// The Registry tab again, saying what was wrong with the last action: a
/// 400 for a refusal of what was asked, a 503 when the producer admin API
/// did not answer - on the page the action came from, never a bare error.
async fn registry_refused(
    app: &App,
    session: &Session,
    error: String,
    unavailable: bool,
) -> PageResult {
    let outcome = AdminOutcome {
        notice: None,
        error: Some(error),
    };
    let mut response = html(&render_admin_registry(app, session, outcome).await?)?;
    *response.status_mut() = if unavailable {
        StatusCode::SERVICE_UNAVAILABLE
    } else {
        StatusCode::BAD_REQUEST
    };
    Ok(response)
}

/// The page-side answer to a failed registry write: the database's own
/// verdict (an unknown repository id, an out-of-range value, the missing
/// producer schema) is a form error; a missing admin tier says so; anything
/// else is a page fault.
async fn registry_write_failed(
    app: &App,
    session: &Session,
    error: anyhow::Error,
    function: &str,
) -> PageResult {
    if crate::db::classify(&error) == Failure::InvalidInput {
        return registry_refused(
            app,
            session,
            crate::db::db_message(&error)
                .unwrap_or_else(|| "The catalog rejected a request value.".to_owned()),
            false,
        )
        .await;
    }
    if is_sql_state(&error, "42501") {
        return registry_refused(
            app,
            session,
            format!(
                "Registry changes need the writer connection to hold the pgokf_admin tier: \
                 {function} is admin-only."
            ),
            false,
        )
        .await;
    }
    Err(AppError::from(error))
}

/// The credential types the producer's admin API accepts (`ssh_key` is
/// deliberately not offered: the producer refuses it with a typed 400).
const CREDENTIAL_KINDS: [&str; 2] = ["github_pat", "http_basic"];

/// The producer's label bound (`CredentialPutRequest.label`, max 200).
const MAX_CREDENTIAL_LABEL: usize = 200;

/// The producer's registration bounds (`normalize_registration`): the
/// registry columns cap the branch and the project name at 255; the
/// checkout path is TEXT with a 1024 sanity cap.
const MAX_BRANCH: usize = 255;
const MAX_PROJECT_NAME: usize = 255;
const MAX_CHECKOUT_PATH: usize = 1024;

/// The poll-interval bounds `pgokf.registry_set_poll_interval` enforces;
/// checked here first so a bad value never reaches the database.
const MIN_POLL_SECONDS: i32 = 5;
const MAX_POLL_SECONDS: i32 = 86400;

#[derive(Debug, Deserialize)]
struct AdminRegistryForm {
    #[serde(default)]
    action: String,
    /// The repository id (a UUID) the action is on.
    #[serde(default)]
    id: String,
    /// `register`: the https remote URL (never with embedded credentials).
    #[serde(default)]
    remote: String,
    /// `register`: the default branch (`main` when left blank).
    #[serde(default)]
    branch: String,
    /// `register`: the project name.
    #[serde(default)]
    project: String,
    /// `register`: where the producer stages its worktree.
    #[serde(default)]
    checkout_path: String,
    /// `register`: the optional first credential's label.
    #[serde(default)]
    label: String,
    /// `register`: the optional credential's type (one of
    /// `CREDENTIAL_KINDS`).
    #[serde(default)]
    kind: String,
    /// `register`: the optional credential's secret. It transits this
    /// request once, into the producer call; it is never stored, logged,
    /// or rendered here.
    #[serde(default)]
    secret: String,
    /// `poll`: the new interval, in seconds.
    #[serde(default)]
    poll_interval: String,
}

/// The repository page's form: the credential actions that used to sit
/// inline in the Registry table live on the repository's own page now.
#[derive(Debug, Deserialize)]
struct AdminRepositoryForm {
    #[serde(default)]
    action: String,
    /// `set-credential`: the label.
    #[serde(default)]
    label: String,
    /// `set-credential`: the credential type (one of `CREDENTIAL_KINDS`).
    #[serde(default)]
    kind: String,
    /// `set-credential`: the secret. It transits this request once, into
    /// the producer call; it is never stored, logged, or rendered here.
    #[serde(default)]
    secret: String,
}

/// The producer admin API, or the honest 503 a credential action answers
/// with when this server was not configured with one.
fn producer_for(app: &App) -> Result<&crate::producer::ProducerAdmin, AppError> {
    app.producer.as_ref().ok_or_else(|| {
        AppError::unavailable(
            "Credential management needs the producer admin API; this server has neither \
             OKF_PRODUCER_ADMIN_URL nor OKF_PRODUCER_ADMIN_TOKEN.",
        )
    })
}

/// Pause or resume one repository (`pgokf.registry_set_status` through the
/// writer connection, whose role must hold the `pgokf_admin` tier).
async fn registry_pause_resume(
    app: &App,
    session: &Session,
    person: &Principal,
    id: &str,
    paused: bool,
) -> Result<String, PageResult> {
    let access = require(app, session, Role::Admin, "/admin/registry").map_err(Err)?;
    let status = if paused { "paused" } else { "active" };
    match access.writer.registry_set_status(id, status).await {
        Ok(()) => {
            eprintln!(
                "pgokf-web: {} {status} registry repository {id}",
                person.actor()
            );
            Ok(format!(
                "Repository {id} {}.",
                if paused { "paused" } else { "resumed" }
            ))
        }
        Err(error) => {
            Err(registry_write_failed(app, session, error, "pgokf.registry_set_status").await)
        }
    }
}

/// Set one repository's poll interval, validated here first so a bad value
/// never reaches the database.
async fn registry_poll_interval(
    app: &App,
    session: &Session,
    person: &Principal,
    id: &str,
    raw: &str,
) -> Result<String, PageResult> {
    let access = require(app, session, Role::Admin, "/admin/registry").map_err(Err)?;
    let Some(seconds) = raw
        .trim()
        .parse::<i32>()
        .ok()
        .filter(|n| (MIN_POLL_SECONDS..=MAX_POLL_SECONDS).contains(n))
    else {
        return Err(registry_refused(
            app,
            session,
            format!(
                "The poll interval must be a number of seconds between {MIN_POLL_SECONDS} and \
                 {MAX_POLL_SECONDS}."
            ),
            false,
        )
        .await);
    };
    match access.writer.registry_set_poll_interval(id, seconds).await {
        Ok(()) => {
            eprintln!(
                "pgokf-web: {} set the poll interval of registry repository {id} to {seconds}s",
                person.actor()
            );
            Ok(format!(
                "Repository {id} now polls every {seconds} seconds."
            ))
        }
        Err(error) => {
            Err(
                registry_write_failed(app, session, error, "pgokf.registry_set_poll_interval")
                    .await,
            )
        }
    }
}

/// The form error for a repository id this server's tenant cannot act on:
/// an unknown id, a malformed one, and another tenant's id all answer
/// identically, so the page never reveals that another tenant's row exists.
const REPOSITORY_NOT_VISIBLE: &str =
    "The registry lists no repository with that id for this catalog.";

/// Gate a credential action on the tenant boundary: the id must name a
/// registry row the session tenant can see, checked before anything -
/// label, type, least of all the secret - is forwarded to the producer.
async fn require_visible_repository(
    app: &App,
    session: &Session,
    id: &str,
) -> Result<(), PageResult> {
    match registry_repository_visible(app, id).await {
        Ok(true) => Ok(()),
        Ok(false) => {
            Err(
                repository_refused(app, session, id, REPOSITORY_NOT_VISIBLE.to_owned(), false)
                    .await,
            )
        }
        Err(error) => Err(Err(error)),
    }
}

/// Validate one credential form (label, type, secret) against the
/// producer's bounds, so an invalid one stops before anything crosses to
/// the producer. The secret passes byte-for-byte as submitted (leading
/// and trailing whitespace may be significant); only an all-whitespace
/// one is refused, since the producer requires a non-empty secret.
fn check_credential(label: &str, kind: &str, secret: &str) -> Result<(), &'static str> {
    if label.is_empty() || label.chars().count() > MAX_CREDENTIAL_LABEL {
        return Err("Give the credential a label (at most 200 characters).");
    }
    if !CREDENTIAL_KINDS.contains(&kind) {
        return Err("Choose a credential type from the list.");
    }
    if secret.trim().is_empty() {
        return Err("Give the credential's secret.");
    }
    Ok(())
}

/// Create or replace a repository's fetch credential through the producer's
/// admin API. The secret transits this request once; the audit line names
/// the label and type, never the secret.
async fn registry_set_credential(
    app: &App,
    session: &Session,
    person: &Principal,
    id: &str,
    label: &str,
    kind: &str,
    secret: &str,
) -> Result<String, PageResult> {
    let producer = producer_for(app).map_err(Err)?;
    require_visible_repository(app, session, id).await?;
    if let Err(why) = check_credential(label, kind, secret) {
        return Err(repository_refused(app, session, id, why.to_owned(), false).await);
    }
    match producer.set_credential(id, label, kind, secret).await {
        Ok(()) => {
            eprintln!(
                "pgokf-web: {} set a {kind} credential ({label}) on registry repository {id}",
                person.actor()
            );
            Ok(format!(
                "Credential {label} ({kind}) is set for repository {id}; only its last four \
                 characters are ever shown."
            ))
        }
        Err(error) => Err(repository_producer_failed(app, session, id, error).await),
    }
}

/// Remove a repository's credential through the producer's admin API,
/// returning it to anonymous fetches.
async fn registry_remove_credential(
    app: &App,
    session: &Session,
    person: &Principal,
    id: &str,
) -> Result<String, PageResult> {
    let producer = producer_for(app).map_err(Err)?;
    require_visible_repository(app, session, id).await?;
    match producer.remove_credential(id).await {
        Ok(()) => {
            eprintln!(
                "pgokf-web: {} removed the credential of registry repository {id}",
                person.actor()
            );
            Ok(format!(
                "Repository {id} has no credential now; it fetches anonymously."
            ))
        }
        Err(error) => Err(repository_producer_failed(app, session, id, error).await),
    }
}

/// The remote-URL rules the producer's `validate_remote_url` enforces,
/// checked here first so a bad value earns a form error instead of a
/// producer round-trip: an explicit `https://` URL, and never one with
/// credentials embedded in it (a credential is set separately, in the
/// credential fields - never in the URL).
fn registration_remote_error(remote: &str) -> Option<&'static str> {
    if remote.is_empty() {
        return Some("Give the repository's remote URL.");
    }
    if !remote.to_ascii_lowercase().starts_with("https://") {
        return Some(
            "The remote URL must be an explicit https:// URL; ssh, git, and http forms are \
             not accepted.",
        );
    }
    let authority = remote["https://".len()..]
        .split('/')
        .next()
        .unwrap_or_default();
    if authority.contains('@') {
        return Some(
            "The remote URL must not embed credentials; give the credential in the credential \
             fields instead.",
        );
    }
    None
}

/// The registry key derived from the remote URL, mirroring the producer's
/// own derivation (its `default_repository_key`): the last two path
/// segments (owner/repository, a trailing `.git` dropped), lowercased,
/// every non-alphanumeric run collapsed to one `-`, trimmed, capped at 64
/// characters. The host is the fallback for a URL with no usable path.
fn derive_repository_key(remote: &str) -> Option<String> {
    let after_scheme = remote.split_once("://").map(|(_, rest)| rest)?;
    let without_query = after_scheme.split(['?', '#']).next().unwrap_or_default();
    let (host, path) = without_query.split_once('/').unwrap_or((without_query, ""));
    let segments: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    let base = match segments.as_slice() {
        [] => host.to_owned(),
        [only] => only.strip_suffix(".git").unwrap_or(only).to_owned(),
        [.., owner, name] => format!("{owner}-{}", name.strip_suffix(".git").unwrap_or(name)),
    };
    let mut slug = String::new();
    let mut pending_dash = false;
    for c in base.chars().flat_map(char::to_lowercase) {
        if c.is_ascii_alphanumeric() {
            if pending_dash && !slug.is_empty() {
                slug.push('-');
            }
            pending_dash = false;
            slug.push(c);
        } else {
            pending_dash = true;
        }
    }
    slug.truncate(64);
    (!slug.is_empty()).then_some(slug)
}

/// The registration receipt's graph resolution, phrased for the notice:
/// every outcome is a success state (an idempotent re-registration is not
/// an error), and `adopted` says plainly that the graph was not shaped by
/// this registration.
fn graph_outcome_notice(receipt: &RegistrationReceipt, project: &str) -> String {
    if receipt.already_registered {
        return format!(
            "Repository {project} is already registered; nothing changed (the existing \
             registration stands)."
        );
    }
    match receipt.graph_outcome.as_str() {
        "created" => format!("Repository {project} registered; a new graph was created for it."),
        "reused" => {
            format!("Repository {project} registered against the project name's existing graph.")
        }
        "adopted" => format!(
            "Repository {project} registered; it adopted a pre-existing graph whose roots \
             differ from this checkout path - confirm that is intended."
        ),
        "provided" => format!("Repository {project} registered against the provided graph."),
        other => format!("Repository {project} registered (graph outcome: {other})."),
    }
}

/// The page-side answer to a refused or failed registration: the 409
/// guards (cross-tenant re-registration, graph-adoption branch mismatch)
/// and the 429 rate limit are phrased for what they mean, an outage is an
/// honest 503 - never a raw dump of the producer's answer. Shown on the
/// add-repository page, where the form is.
fn registration_failed(app: &App, session: &Session, error: &ProducerError) -> PageResult {
    let (message, unavailable) = match error {
        ProducerError::Rejected(status) if *status == StatusCode::CONFLICT => (
            "The producer refused the registration (HTTP 409): this key and branch are already \
             registered under another tenant, or the project name collides with an existing \
             graph on a different branch. Re-registering the same repository is not an error; \
             a different branch or project name resolves the collision."
                .to_owned(),
            false,
        ),
        ProducerError::Rejected(status) if *status == StatusCode::TOO_MANY_REQUESTS => (
            "The producer's admin API rate limit was reached (HTTP 429); wait a minute and \
             try again."
                .to_owned(),
            true,
        ),
        _ => (error.message(), error.is_unavailable()),
    };
    repository_new_refused(app, session, message, unavailable)
}

/// The register form, validated against the producer's bounds and ready
/// for the producer call.
struct RegistrationDraft<'a> {
    remote: &'a str,
    /// The registry key derived from the remote URL's slug.
    key: String,
    branch: String,
    project: &'a str,
    checkout_path: &'a str,
    /// The optional first credential: `(label, kind, secret)`, present only
    /// when all three credential fields are filled.
    credential: Option<(&'a str, &'a str, &'a str)>,
}

/// Validate the register form, so a bad value earns a form error before
/// anything crosses to the producer.
fn registration_draft(form: &AdminRegistryForm) -> Result<RegistrationDraft<'_>, &'static str> {
    let remote = form.remote.trim();
    if let Some(why) = registration_remote_error(remote) {
        return Err(why);
    }
    let branch = non_empty(&form.branch).unwrap_or_else(|| "main".to_owned());
    if branch.chars().count() > MAX_BRANCH {
        return Err("The branch name must be at most 255 characters.");
    }
    let project = form.project.trim();
    if project.is_empty() || project.chars().count() > MAX_PROJECT_NAME {
        return Err("Give the project name (at most 255 characters).");
    }
    let checkout_path = form.checkout_path.trim();
    if checkout_path.is_empty() || checkout_path.chars().count() > MAX_CHECKOUT_PATH {
        return Err(
            "Give the checkout path - where the producer stages its worktree (at most 1024 \
             characters).",
        );
    }
    let Some(key) = derive_repository_key(remote) else {
        return Err("Could not derive a registry key from that remote URL.");
    };
    // The credential is optional: all three fields, or none of them.
    let label = form.label.trim();
    let kind = form.kind.trim();
    let credential = if label.is_empty() && kind.is_empty() && form.secret.is_empty() {
        None
    } else {
        check_credential(label, kind, &form.secret)?;
        Some((label, kind, form.secret.as_str()))
    };
    Ok(RegistrationDraft {
        remote,
        key,
        branch,
        project,
        checkout_path,
        credential,
    })
}

/// Register a repository through the producer's admin API: the
/// registration itself, then - when the form's credential fields are
/// filled - the credential, so the repository's first reconcile is
/// already authenticated. Registration is idempotent at the producer: a
/// repeat is reported as the existing row, not an error. The answer lands
/// on the new repository's own page.
async fn registry_register(
    app: &App,
    session: &Session,
    person: &Principal,
    form: &AdminRegistryForm,
) -> PageResult {
    let producer = producer_for(app)?;
    let draft = match registration_draft(form) {
        Ok(draft) => draft,
        Err(why) => return repository_new_refused(app, session, why.to_owned(), false),
    };
    let receipt = match producer
        .register(
            draft.remote,
            &draft.key,
            &draft.branch,
            draft.checkout_path,
            draft.project,
            app.tenant.as_deref(),
        )
        .await
    {
        Ok(receipt) => receipt,
        Err(error) => return registration_failed(app, session, &error),
    };
    eprintln!(
        "pgokf-web: {} registered registry repository {} ({}, {}, graph {})",
        person.actor(),
        receipt.repository_id,
        draft.key,
        draft.branch,
        receipt.graph_outcome
    );
    let mut notice = graph_outcome_notice(&receipt, draft.project);
    if let Some((label, kind, secret)) = draft.credential {
        match producer
            .set_credential(&receipt.repository_id, label, kind, secret)
            .await
        {
            Ok(()) => {
                eprintln!(
                    "pgokf-web: {} set a {kind} credential ({label}) on registry repository {}",
                    person.actor(),
                    receipt.repository_id
                );
                notice = format!(
                    "{notice} The credential {label} ({kind}) is set, so the first reconcile \
                     is already authenticated."
                );
            }
            // The repository IS registered; only the credential did not
            // land. Say exactly that, and where to finish the job.
            Err(error) => {
                return repository_new_refused(
                    app,
                    session,
                    format!(
                        "Repository {} registered (id {}), but setting its credential \
                         failed: {} Set it from the repository's page.",
                        draft.project,
                        receipt.repository_id,
                        error.message()
                    ),
                    error.is_unavailable(),
                );
            }
        }
    }
    Ok(redirect(&format!(
        "/admin/registry/{}?notice={}",
        filters::percent_encode(&receipt.repository_id),
        filters::percent_encode(&notice)
    )))
}

async fn admin_registry(
    State(app): State<Shared>,
    session: Session,
    Form(form): Form<AdminRegistryForm>,
) -> PageResult {
    let person = admin(&session)?;
    // Registration is the one action that addresses no existing
    // repository; its answer lands on the new repository's own page.
    if form.action == "register" {
        return registry_register(&app, &session, &person, &form).await;
    }
    let id = form.id.trim().to_owned();
    if id.is_empty() {
        return Err(AppError::bad_request("Choose a repository."));
    }
    let notice = match form.action.as_str() {
        "pause" => registry_pause_resume(&app, &session, &person, &id, true).await,
        "resume" => registry_pause_resume(&app, &session, &person, &id, false).await,
        "poll" => registry_poll_interval(&app, &session, &person, &id, &form.poll_interval).await,
        other => return Err(AppError::bad_request(format!("Unknown action {other:?}."))),
    };
    match notice {
        Ok(notice) => Ok(AdminTab::Registry.redirect_with(&notice)),
        Err(result) => result,
    }
}

/// One repository's page: the registry row (tenant-scoped, from the same
/// read the Registry tab lists), the credential as the producer reports
/// it, and the checkout path from the producer's operator API (the column
/// the database reader grant deliberately withholds). An id outside the
/// session tenant is the same 404 as an unknown one - the page never
/// reveals that another tenant's row exists.
async fn render_admin_repository(
    app: &App,
    session: &Session,
    id: &str,
    outcome: AdminOutcome,
) -> Result<AdminRepositoryPage, AppError> {
    let repos = registry_repositories(app).await?;
    let Some(repo) = repos.into_iter().find(|repo| repo.id == id) else {
        return Err(AppError::not_found("This repository"));
    };
    let producer_configured = app.producer.is_some();
    let mut credentials_known = true;
    let mut credential_note = None;
    let cell = match app.producer.as_ref() {
        Some(producer) => match producer.credential(id).await {
            Ok(Some(info)) => CredentialCell::set(info),
            Ok(None) => CredentialCell::anonymous(),
            Err(ProducerError::Unavailable) => {
                credentials_known = false;
                credential_note = Some(
                    "The producer admin API did not answer, so the credential state and the \
                     checkout path are unknown - reload to try again. The registry row itself \
                     (read from the database) is unaffected."
                        .to_owned(),
                );
                CredentialCell::unknown()
            }
            // A refusal on the credential read marks the one cell.
            Err(_) => CredentialCell::unknown(),
        },
        None => CredentialCell::unknown(),
    };
    let checkout_path = match app.producer.as_ref() {
        Some(producer) if credentials_known => match producer.repository(id).await {
            Ok(Some(detail)) => Some(detail.checkout_path),
            Ok(None) | Err(_) => None,
        },
        _ => None,
    };
    if !producer_configured {
        credential_note = Some(
            "Credential management needs the producer admin API (OKF_PRODUCER_ADMIN_URL and \
             OKF_PRODUCER_ADMIN_TOKEN together); without them the credential state and the \
             checkout path read as unknown here."
                .to_owned(),
        );
    }
    let title = format!("Administration · {}", repo.project);
    Ok(AdminRepositoryPage {
        shell: Shell::new(app, session, &title, "admin"),
        admin: AdminTab::Registry.shell(outcome),
        repo: RepositoryDetailView::new(repo, cell),
        producer_configured,
        credentials_known: credentials_known && producer_configured,
        credential_note,
        checkout_path,
    })
}

async fn admin_repository_page(
    State(app): State<Shared>,
    session: Session,
    Path(id): Path<String>,
    Query(params): Query<NoticeParams>,
) -> PageResult {
    admin(&session)?;
    let outcome = AdminOutcome {
        notice: non_empty(&params.notice),
        error: None,
    };
    html(&render_admin_repository(&app, &session, &id, outcome).await?)
}

/// The repository page again, saying what was wrong with the last action:
/// a 400 for a refusal of what was asked, a 503 when the producer admin
/// API did not answer - on the page the action came from, never a bare
/// error. (When the id names nothing this tenant can see, the refusal
/// falls back to the Registry tab: the page itself is a 404 then.)
async fn repository_refused(
    app: &App,
    session: &Session,
    id: &str,
    error: String,
    unavailable: bool,
) -> PageResult {
    let outcome = AdminOutcome {
        notice: None,
        error: Some(error.clone()),
    };
    match render_admin_repository(app, session, id, outcome).await {
        Ok(page) => {
            let mut response = html(&page)?;
            *response.status_mut() = if unavailable {
                StatusCode::SERVICE_UNAVAILABLE
            } else {
                StatusCode::BAD_REQUEST
            };
            Ok(response)
        }
        Err(_) => registry_refused(app, session, error, unavailable).await,
    }
}

/// The page-side answer to a failed producer call from a repository page:
/// an outage renders the page with a 503, a refusal with a 400, each
/// saying what happened.
async fn repository_producer_failed(
    app: &App,
    session: &Session,
    id: &str,
    error: ProducerError,
) -> PageResult {
    let unavailable = error.is_unavailable();
    repository_refused(app, session, id, error.message(), unavailable).await
}

async fn admin_repository(
    State(app): State<Shared>,
    session: Session,
    Path(id): Path<String>,
    Form(form): Form<AdminRepositoryForm>,
) -> PageResult {
    let person = admin(&session)?;
    let id = id.trim().to_owned();
    if id.is_empty() {
        return Err(AppError::bad_request("Choose a repository."));
    }
    let notice = match form.action.as_str() {
        "set-credential" => {
            registry_set_credential(
                &app,
                &session,
                &person,
                &id,
                form.label.trim(),
                form.kind.trim(),
                &form.secret,
            )
            .await
        }
        "remove-credential" => registry_remove_credential(&app, &session, &person, &id).await,
        other => return Err(AppError::bad_request(format!("Unknown action {other:?}."))),
    };
    match notice {
        Ok(notice) => Ok(redirect(&format!(
            "/admin/registry/{}?notice={}",
            filters::percent_encode(&id),
            filters::percent_encode(&notice)
        ))),
        Err(result) => result,
    }
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
    let graph = app
        .db
        .graph(bundle_id, &concept_id, hops, EdgeSource::Both)
        .await?;
    if graph.nodes.is_empty() {
        return Err(AppError::not_found("This concept"));
    }
    Ok(Json(graph_json(
        Some((bundle_id, &concept_id)),
        hops,
        &graph,
        ColorBy::Hops,
        EdgeSource::Both,
    )))
}

/// Nodes drawn by the catalog-wide explorer at most, and by default.
const GRAPH_MAX_NODES: i32 = 2000;
const GRAPH_DEFAULT_NODES: i32 = 300;
const GRAPH_NODE_OPTIONS: [i32; 5] = [100, 300, 600, 1000, 2000];
/// The sidebar's edge-source choices as `(value, label)`; the default -
/// both sources, since typed relationships are the point of drawing them -
/// is first.
const GRAPH_EDGE_OPTIONS: [(&str, &str); 3] = [
    ("both", "Links and relationships"),
    ("links", "Document links only"),
    ("rels", "Relationships only"),
];

#[derive(Debug, Default)]
struct CatalogGraphParams {
    /// Every `bundle` value, in order: `?bundle=2&bundle=5` selects several
    /// bundles, one value still works, and none (or an empty one) means all.
    bundles: Vec<String>,
    /// Every `type` value, in order: each is an exact type string, matched
    /// verbatim; none (or an empty one) means all types.
    types: Vec<String>,
    /// Every `type_group` value, in order: each a group slug in its own
    /// parameter, so an exact type string is never read as a group.
    type_groups: Vec<String>,
    limit: String,
    /// Which edge sources to draw: `links`, `rels`, or `both` (the default).
    edges: String,
    /// `bundle_id:concept_id` to draw a neighborhood instead.
    seed: String,
    hops: String,
}

impl CatalogGraphParams {
    /// Parse the raw query string. This cannot go through `Query<T>`:
    /// `serde_urlencoded` rejects a repeated scalar field with
    /// `duplicate field bundle`, so multi-selection would 400. `bundle` and
    /// `type` collect every value; the remaining keys are last-wins. Keys
    /// and values are both percent-decoded, as form encoding allows either.
    fn parse(query: Option<&str>) -> Self {
        let mut params = Self::default();
        let Some(query) = query else { return params };
        for pair in query.split('&') {
            let Some((key, value)) = pair.split_once('=') else {
                continue;
            };
            let value = filters::percent_decode(value);
            match filters::percent_decode(key).as_str() {
                "bundle" => params.bundles.push(value),
                "type" => params.types.push(value),
                "type_group" => params.type_groups.push(value),
                "limit" => params.limit = value,
                "edges" => params.edges = value,
                "seed" => params.seed = value,
                "hops" => params.hops = value,
                _ => {}
            }
        }
        params
    }

    /// The selected bundle ids. Empty values (the legacy "all" option's
    /// submission) are ignored, surrounding whitespace is trimmed, and
    /// duplicates collapse to their first occurrence so a repeated id
    /// behaves exactly like a single one.
    fn bundle_ids(&self) -> Result<Vec<i64>, AppError> {
        let mut ids = Vec::new();
        for raw in &self.bundles {
            let trimmed = raw.trim();
            if trimmed.is_empty() {
                continue;
            }
            let id = trimmed
                .parse::<i64>()
                .map_err(|_| AppError::bad_request("bundle must be an integer id"))?;
            if !ids.contains(&id) {
                ids.push(id);
            }
        }
        Ok(ids)
    }

    /// The raw `type` values (exact type strings), trimmed, empties
    /// dropped, duplicates collapsed to their first occurrence - the same
    /// normalisation [`Self::bundle_ids`] applies.
    fn type_values(&self) -> Vec<String> {
        normalized(&self.types)
    }

    /// The raw `type_group` values (group slugs), normalised like
    /// [`Self::type_values`].
    fn group_slugs(&self) -> Vec<String> {
        normalized(&self.type_groups)
    }

    fn limit(&self) -> i32 {
        self.limit
            .parse::<i32>()
            .ok()
            .filter(|n| (1..=GRAPH_MAX_NODES).contains(n))
            .unwrap_or(GRAPH_DEFAULT_NODES)
    }

    /// The selected edge sources. Absent or empty means the default
    /// [`EdgeSource::Both`]; an unknown value is rejected rather than
    /// silently widened back to the default.
    fn edge_source(&self) -> Result<EdgeSource, AppError> {
        match non_empty(&self.edges).as_deref() {
            None | Some("both") => Ok(EdgeSource::Both),
            Some("links") => Ok(EdgeSource::Links),
            Some("rels") => Ok(EdgeSource::Relationships),
            Some(_) => Err(AppError::bad_request("edges must be links, rels, or both")),
        }
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

/// Trim, drop empties, and collapse duplicates to their first occurrence.
fn normalized(raw: &[String]) -> Vec<String> {
    let mut values: Vec<String> = Vec::new();
    for value in raw {
        let trimmed = value.trim();
        if !trimmed.is_empty() && !values.iter().any(|v| v == trimmed) {
            values.push(trimmed.to_owned());
        }
    }
    values
}

/// Expand the graph's selections - exact `type` values, matched verbatim,
/// and `type_group` slugs - into the exact types the SQL filters by.
/// `observed` carries the catalog's complete type inventory, so groups
/// match what the checkbox list offered, including types beyond the
/// display facets' cap. The flag is `true` when filters were given but
/// expand to nothing (a group with no observed members): the graph must
/// come back empty, never unfiltered.
fn expand_graph_types(
    exact: &[String],
    groups: &[String],
    observed: &[String],
) -> Result<(Vec<String>, bool), AppError> {
    let mut types: Vec<String> = Vec::new();
    let mut filtered = false;
    for concept_type in exact {
        filtered = true;
        if !types.contains(concept_type) {
            types.push(concept_type.clone());
        }
    }
    for slug in groups {
        filtered = true;
        let expanded = type_groups::expand_group(slug, observed)
            .ok_or_else(|| AppError::bad_request("unknown type group"))?;
        for concept_type in expanded {
            if !types.contains(&concept_type) {
                types.push(concept_type);
            }
        }
    }
    let impossible = filtered && types.is_empty();
    Ok((types, impossible))
}

/// [`expand_graph_types`] with the catalog's complete type inventory read
/// first (only when a group slug needs it: exact types pass through).
async fn resolve_graph_types(
    app: &App,
    exact: &[String],
    groups: &[String],
) -> Result<(Vec<String>, bool), AppError> {
    let observed = if groups.is_empty() {
        Vec::new()
    } else {
        app.db.catalog_types(None).await?
    };
    expand_graph_types(exact, groups, &observed)
}

/// The graph page's type checkboxes: one per observed group (unknown types
/// appear under "Other", so no type is ever unfilterable), plus any
/// selected group nothing observed and any selected exact type, keeping a
/// round-tripped choice visible - and checked, so resubmitting keeps the
/// filter instead of silently dropping it.
fn graph_type_filters(
    observed: &[Facet],
    selected_types: &[String],
    selected_groups: &[String],
) -> Vec<GraphTypeFilter> {
    let mut filters: Vec<GraphTypeFilter> = type_groups::groups_from_facets(observed)
        .into_iter()
        .map(|g| GraphTypeFilter {
            selected: selected_groups.iter().any(|s| s == g.slug),
            label: g.label.to_owned(),
            value: g.slug.to_owned(),
            group: true,
        })
        .collect();
    for slug in selected_groups {
        if type_groups::is_known_slug(slug) && !filters.iter().any(|f| f.group && f.value == *slug)
        {
            filters.push(GraphTypeFilter {
                label: type_groups::label_of(slug),
                value: slug.clone(),
                selected: true,
                group: true,
            });
        }
    }
    for concept_type in selected_types {
        filters.push(GraphTypeFilter {
            label: concept_type.clone(),
            value: concept_type.clone(),
            selected: true,
            group: false,
        });
    }
    filters
}

/// The catalog-wide graph (or a seeded neighborhood) for the explorer.
async fn api_catalog_graph(
    State(app): State<Shared>,
    RawQuery(query): RawQuery,
) -> Result<Json<Value>, AppError> {
    let params = CatalogGraphParams::parse(query.as_deref());
    let bundle_ids = params.bundle_ids()?;
    let edges = params.edge_source()?;
    if let Some((seed_bundle, seed_id)) = params.seed()? {
        let hops = parse_hops(&params.hops);
        let graph = app.db.graph(seed_bundle, &seed_id, hops, edges).await?;
        if graph.nodes.is_empty() {
            return Err(AppError::not_found("This concept"));
        }
        return Ok(Json(graph_json(
            Some((seed_bundle, &seed_id)),
            hops,
            &graph,
            ColorBy::Hops,
            edges,
        )));
    }
    let (types, impossible) =
        resolve_graph_types(&app, &params.type_values(), &params.group_slugs()).await?;
    let graph = if impossible {
        Graph::default()
    } else {
        app.db
            .catalog_graph(&bundle_ids, &types, i64::from(params.limit()), edges)
            .await?
    };
    let color_by = if bundle_ids.len() == 1 {
        ColorBy::Type
    } else {
        ColorBy::Bundle
    };
    Ok(Json(graph_json(None, 0, &graph, color_by, edges)))
}

async fn graph_page(
    State(app): State<Shared>,
    session: Session,
    RawQuery(query): RawQuery,
) -> PageResult {
    let params = CatalogGraphParams::parse(query.as_deref());
    let bundle_ids = params.bundle_ids()?;
    let type_values = params.type_values();
    let group_slugs = params.group_slugs();
    let edges = params.edge_source()?;
    let seed = params.seed()?;
    let limit = params.limit();
    let bundles = app.db.bundles().await?;
    let type_facets = app.db.catalog_facets(None, "type").await?;
    let type_filters = graph_type_filters(&type_facets, &type_values, &group_slugs);
    let hops = parse_hops(&params.hops);
    let seed_label = match &seed {
        Some((b, id)) => Some(
            app.db
                .concept(*b, id)
                .await?
                .and_then(|c| c.title)
                .unwrap_or_else(|| id.clone()),
        ),
        None => None,
    };
    html(&GraphPage {
        shell: Shell::new(&app, &session, "Graph", "graph"),
        bundles,
        bundle_ids: bundle_ids.clone(),
        type_filters,
        limit_options: GRAPH_NODE_OPTIONS
            .iter()
            .map(|n| (*n, *n == limit))
            .collect(),
        edge_options: GRAPH_EDGE_OPTIONS
            .iter()
            .map(|(value, label)| (*value, *value == edges.as_str(), *label))
            .collect(),
        graph_url: format!(
            "/api/graph?{}",
            graph_url_query(
                &bundle_ids,
                &type_values,
                &group_slugs,
                limit,
                seed.as_ref(),
                edges,
            )
        ),
        catalog_url: format!(
            "/graph?{}",
            graph_url_query(&bundle_ids, &type_values, &group_slugs, limit, None, edges)
        ),
        hops,
        seed_label,
    })
}

/// The explorer endpoint's query string. `bundle`, `type`, and
/// `type_group` repeat once per selection so every choice reaches the API
/// (a form GET submits the checked boxes the same way); exact types and
/// group slugs keep their separate parameters. The edge source always
/// round-trips, so a redrawn form and a shared link select the same edges.
fn graph_url_query(
    bundle_ids: &[i64],
    types: &[String],
    groups: &[String],
    limit: i32,
    seed: Option<&(i64, String)>,
    edges: EdgeSource,
) -> String {
    let mut pairs: Vec<String> = bundle_ids.iter().map(|b| format!("bundle={b}")).collect();
    pairs.extend(
        types
            .iter()
            .map(|t| format!("type={}", filters::percent_encode(t))),
    );
    pairs.extend(
        groups
            .iter()
            .map(|g| format!("type_group={}", filters::percent_encode(g))),
    );
    pairs.push(format!("limit={limit}"));
    pairs.push(format!("edges={}", edges.as_str()));
    if let Some((b, id)) = seed {
        pairs.push(format!(
            "seed={}",
            filters::percent_encode(&format!("{b}:{id}"))
        ));
    }
    pairs.join("&")
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
    /// `warn` (default) or `exclude`: what the build does with concepts the
    /// catalog reports as not fresh.
    #[serde(default)]
    stale_policy: String,
    /// Closure seeds, one `bundle_id:concept_id` per line.
    #[serde(default)]
    seeds: String,
    /// The namespaced relationship types the closure follows (comma-separated).
    #[serde(default)]
    relation_types: String,
    /// `outbound` (default), `inbound`, or `both`.
    #[serde(default)]
    direction: String,
    #[serde(default)]
    hops: String,
    /// `1` when the closure must complete or the build refuses.
    #[serde(default)]
    require_closure: String,
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
    #[allow(clippy::too_many_lines)]
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
        let (seeds, seed_problem) = parse_picks(&self.seeds);
        let stale_policy = match non_empty(&self.stale_policy) {
            None => pgokf_workspace::StalePolicy::Warn,
            Some(id) => pgokf_workspace::StalePolicy::parse(&id)
                .ok_or_else(|| AppError::bad_request("stale_policy must be warn or exclude"))?,
        };
        let direction = match non_empty(&self.direction) {
            None => pgokf_workspace::Direction::Outbound,
            Some(id) => pgokf_workspace::Direction::parse(&id).ok_or_else(|| {
                AppError::bad_request("direction must be outbound, inbound, or both")
            })?,
        };
        let hops = match non_empty(&self.hops) {
            None => None,
            Some(raw) => Some(
                raw.parse::<usize>()
                    .ok()
                    .filter(|n| (1..=pgokf_workspace::MAX_HOPS).contains(n))
                    .ok_or_else(|| {
                        AppError::bad_request(format!(
                            "hops must be between 1 and {}",
                            pgokf_workspace::MAX_HOPS
                        ))
                    })?,
            ),
        };
        let require_closure = on(&self.require_closure);
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
            stale_policy,
            seeds,
            relation_types: split_list(&self.relation_types),
            direction,
            hops,
            require_closure,
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
            stale_policy: selection.stale_policy.id().to_owned(),
            seeds: selection
                .seeds
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join("\n"),
            relation_types: split_list(&self.relation_types).join(", "),
            direction: selection.direction.id().to_owned(),
            hops: selection.hops.map(|n| n.to_string()).unwrap_or_default(),
            require_closure,
            seed_problem,
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
            (
                "stale_policy",
                if selection.stale_policy == pgokf_workspace::StalePolicy::Exclude {
                    "exclude".to_owned()
                } else {
                    String::new()
                },
            ),
            (
                "seeds",
                selection
                    .seeds
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join("\n"),
            ),
            (
                "relation_types",
                split_list(&self.relation_types).join(", "),
            ),
            (
                "direction",
                if selection.direction == pgokf_workspace::Direction::Outbound {
                    String::new()
                } else {
                    selection.direction.id().to_owned()
                },
            ),
            (
                "hops",
                selection.hops.map(|n| n.to_string()).unwrap_or_default(),
            ),
            ("require_closure", flag(selection.require_closure)),
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
/// produce. The resolution runs under the same one-snapshot, freshness-aware
/// policy as the download, so the preview shows exactly what a build would
/// do - including its refusals.
async fn plugin_preview(
    app: &App,
    form: &PluginForm,
    chosen: &Chosen,
    selection: &Selection,
    base_model: Option<String>,
) -> Result<PluginPreview, AppError> {
    let mut client = app.db.checkout().await?;
    let (mut concepts, snapshot, report) =
        pgokf_workspace::resolve_in_transaction(client.client_mut(), selection)
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
    let plugin = if concepts.is_empty() {
        None
    } else {
        Some(
            pgokf_workspace::assemble_with_report(
                &options, selection, &snapshot, &concepts, &report,
            )
            .map_err(workspace_error)?,
        )
    };
    for c in &mut concepts {
        c.bytes.clear();
    }
    let mut mcp_args = serde_json::json!({
        "target": chosen.target.id(),
        "harness": chosen.harness,
        "name": pgokf_workspace::slug(&form.name),
        "all": selection.all,
        "bundle_ids": selection.bundle_ids,
        "types": selection.types,
        "tags": selection.tags,
        "concept_ids": selection.concept_ids,
        "picks": selection.picks.iter().map(ToString::to_string).collect::<Vec<_>>(),
        "seeds": selection.seeds.iter().map(ToString::to_string).collect::<Vec<_>>(),
        "relation_types": selection.relation_types,
        "require_closure": selection.require_closure,
        "query": selection.query,
        "verified_only": selection.verified_only,
        "limit": selection.effective_limit(),
        "components": options.components.iter().map(|c| c.id()).collect::<Vec<_>>(),
        "mcp_command": options.mcp_command,
        "mcp_url": options.mcp_url,
        "web_url": options.web_url,
        "output_dir": "/path/to/your/workspace",
    });
    if selection.stale_policy == pgokf_workspace::StalePolicy::Exclude {
        mcp_args["stale_policy"] = Value::String("exclude".to_owned());
    }
    if !selection.seeds.is_empty() {
        mcp_args["direction"] = Value::String(selection.direction.id().to_owned());
        mcp_args["hops"] = serde_json::json!(selection.effective_hops());
    }
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
        stale_count: plugin.as_ref().map_or(0, |p| p.warnings.len()),
        excluded_count: plugin.as_ref().map_or(0, |p| p.excluded.len()),
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
    let plugin = pgokf_workspace::build_in_transaction(client.client_mut(), &options, &selection)
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

    /// Percent-decode a query-string value (`+` decodes as a space, as in
    /// form encoding). Invalid escapes pass through unchanged.
    pub(crate) fn percent_decode(value: &str) -> String {
        fn hex(byte: u8) -> Option<u8> {
            match byte {
                b'0'..=b'9' => Some(byte - b'0'),
                b'a'..=b'f' => Some(byte - b'a' + 10),
                b'A'..=b'F' => Some(byte - b'A' + 10),
                _ => None,
            }
        }
        let bytes = value.as_bytes();
        let mut out = Vec::with_capacity(bytes.len());
        let mut i = 0;
        while i < bytes.len() {
            match bytes[i] {
                b'+' => {
                    out.push(b' ');
                    i += 1;
                }
                b'%' if i + 2 < bytes.len() => {
                    if let (Some(hi), Some(lo)) = (hex(bytes[i + 1]), hex(bytes[i + 2])) {
                        out.push(hi * 16 + lo);
                        i += 3;
                    } else {
                        out.push(b'%');
                        i += 1;
                    }
                }
                byte => {
                    out.push(byte);
                    i += 1;
                }
            }
        }
        String::from_utf8_lossy(&out).into_owned()
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
    use tower::ServiceExt as _;

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
                "type" => p.concept_type = (*v).to_owned(),
                "type_group" => p.type_group = (*v).to_owned(),
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
            type_group: String::new(),
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
    fn normalize_keeps_the_legacy_exact_type_param() {
        // Arrange
        let p = params(&[
            ("q", "failover"),
            ("bundle", "2"),
            ("type", "Guide"),
            ("mode", "hybrid"),
        ]);

        // Act
        let (form, query) = p.normalize().ok().expect("valid params");

        // Assert: one exact type, no group, everything else combined as before.
        assert_eq!(query.concept_types, vec!["Guide".to_owned()]);
        assert!(query.type_group.is_none());
        assert_eq!(query.bundle_id, Some(2));
        assert_eq!(form.mode, "hybrid");
        assert_eq!(form.concept_type, "Guide");
        assert!(query.has_filters());
    }

    #[test]
    fn normalize_parses_a_type_group_slug_for_later_expansion() {
        // Arrange
        let p = params(&[("q", "failover"), ("type_group", "documents")]);

        // Act
        let (form, query) = p.normalize().ok().expect("valid params");

        // Assert: the slug waits for expansion (no exact types yet), and
        // echoes into the form so the group select can show it selected.
        assert_eq!(query.type_group.as_deref(), Some("documents"));
        assert!(query.concept_types.is_empty());
        assert_eq!(form.type_group, "documents");
        assert!(form.concept_type.is_empty());
        assert!(query.has_filters());
        assert!(query.type_filter_impossible());
    }

    #[test]
    fn normalize_treats_group_prefixed_type_strings_as_verbatim_exact_types() {
        // Arrange: legacy exact type strings that begin with the old
        // in-band group marker - they must keep working verbatim.
        let legacy = params(&[("q", "x"), ("type", "group:Widget")]);
        let colliding = params(&[("q", "x"), ("type", "group:code")]);

        // Act
        let (_, legacy_query) = legacy.normalize().ok().expect("a legacy URL still works");
        let (_, colliding_query) = colliding.normalize().ok().expect("verbatim exact type");

        // Assert: both are exact types, neither is read as a group.
        assert_eq!(legacy_query.concept_types, vec!["group:Widget".to_owned()]);
        assert!(legacy_query.type_group.is_none());
        assert_eq!(colliding_query.concept_types, vec!["group:code".to_owned()]);
        assert!(colliding_query.type_group.is_none());
    }

    #[test]
    fn normalize_rejects_type_and_type_group_together() {
        // Arrange
        let p = params(&[("q", "x"), ("type", "Guide"), ("type_group", "code")]);

        // Act & Assert: accepting both would silently drop one.
        assert!(p.normalize().is_err());
    }

    #[test]
    fn normalize_rejects_an_unknown_type_group_slug() {
        // Arrange
        let p = params(&[("q", "x"), ("type_group", "bogus")]);

        // Act & Assert
        assert!(p.normalize().is_err());
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

    fn type_facet(value: &str, count: i64) -> Facet {
        Facet {
            value: value.to_owned(),
            count,
        }
    }

    #[test]
    fn type_selects_buckets_facets_and_marks_the_selected_group() {
        // Arrange
        let observed = vec![
            type_facet("Code Entity", 12_006),
            type_facet("Guide", 10),
            type_facet("Qualia", 3),
        ];

        // Act
        let selects = type_selects(&observed, "documents", "");

        // Assert: group options carry slugs and counts with the selected
        // group marked, exact types sit under their group's optgroup, and
        // the unknown type is visible under Other.
        let group_options: Vec<(&str, &str, bool)> = selects
            .groups
            .iter()
            .map(|o| (o.value.as_str(), o.label.as_str(), o.selected))
            .collect();
        assert_eq!(
            group_options,
            vec![
                ("code", "Code (12006)", false),
                ("documents", "Documents (10)", true),
                ("other", "Other (3)", false),
            ]
        );
        assert_eq!(selects.types.len(), 3);
        assert_eq!(selects.types[0].label, "Code (12006)");
        assert_eq!(selects.types[0].types[0].label, "Code Entity (12006)");
        assert_eq!(selects.types[2].label, "Other (3)");
        assert_eq!(selects.types[2].types[0].value, "Qualia");
    }

    #[test]
    fn type_selects_marks_a_selected_exact_type() {
        // Arrange
        let observed = vec![type_facet("Code Entity", 12_006), type_facet("Guide", 10)];

        // Act
        let selects = type_selects(&observed, "", "Guide");

        // Assert: the exact type is selected inside its optgroup; no group.
        assert!(selects.groups.iter().all(|o| !o.selected));
        assert!(
            selects.types[1]
                .types
                .iter()
                .any(|o| o.selected && o.value == "Guide")
        );
        assert!(!selects.types[0].types.iter().any(|o| o.selected));
    }

    #[test]
    fn type_selects_keeps_an_unobserved_choice_selectable() {
        // Arrange: a round-tripped group and an exact type nothing observed.
        let observed = vec![type_facet("Guide", 10)];

        // Act
        let with_group = type_selects(&observed, "code", "");
        let with_exact = type_selects(&observed, "", "Widget");

        // Assert: both still render, selected, so they can be cleared.
        let appended_group = with_group.groups.last().expect("the group is appended");
        assert!(appended_group.selected);
        assert_eq!(appended_group.value, "code");
        assert_eq!(appended_group.label, "Code");
        let appended_exact = with_exact.types.last().expect("the type is appended");
        assert_eq!(appended_exact.label, "Selected type");
        assert!(appended_exact.types[0].selected);
        assert_eq!(appended_exact.types[0].value, "Widget");
    }

    #[test]
    fn search_page_renders_the_type_controls_with_selected_state() {
        // Arrange
        let mut form = form("", "", "");
        form.type_group = "documents".to_owned();
        let page = SearchPage {
            shell: Shell::bare("Search"),
            form,
            bundles: Vec::new(),
            facets: FacetsView::empty(),
            type_selects: type_selects(
                &[
                    type_facet("Code Entity", 12_006),
                    type_facet("Guide", 10),
                    type_facet("Runbook", 7),
                ],
                "documents",
                "",
            ),
            results: ResultsView::empty("Search", "Type a query, or pick a filter to browse."),
            semantic_available: false,
            backend_label: "native".to_owned(),
        };

        // Act
        let rendered = page.render().expect("the search page renders");

        // Assert: two selects inside the refreshable field - the group
        // select submits the separate `type_group` parameter with the
        // selected slug marked, the exact-type select submits the legacy
        // `type` parameter verbatim, optgrouped by display group.
        assert!(rendered.contains("id=\"pgokf-type-filter\""));
        assert!(rendered.contains("<select id=\"f-type-group\" name=\"type_group\">"));
        assert!(rendered.contains("<option value=\"documents\" selected>Documents (17)</option>"));
        assert!(rendered.contains("<select id=\"f-type\" name=\"type\">"));
        assert!(rendered.contains("<optgroup label=\"Code (12006)\">"));
        assert!(rendered.contains("<option value=\"Guide\">Guide (10)</option>"));
        assert!(!rendered.contains("datalist"));
    }

    #[test]
    fn results_partial_refreshes_the_type_controls_out_of_band() {
        // Arrange: a partial response with a group selected.
        let mut form = form("", "", "");
        form.type_group = "code".to_owned();
        let partial = ResultsPartial {
            form,
            facets: FacetsView::empty(),
            type_selects: type_selects(
                &[type_facet("Code Entity", 12_006), type_facet("Guide", 10)],
                "code",
                "",
            ),
            results: ResultsView::empty("Search", "No matches."),
            oob: true,
        };

        // Act
        let refreshed = partial.render().expect("the partial renders");

        // Assert: the type controls swap out of band with the facets, the
        // selected group preserved.
        assert!(
            refreshed.contains("<div hx-swap-oob=\"true\" id=\"pgokf-type-filter\">"),
            "the type filter swaps out of band"
        );
        assert!(refreshed.contains("<option value=\"code\" selected>Code (12006)</option>"));
    }

    #[test]
    fn results_partial_refreshes_the_filter_toggle_out_of_band() {
        // Arrange: partial responses, one with a non-default filter, one
        // without.
        let partial_with = |form: SearchForm| ResultsPartial {
            form,
            facets: FacetsView::empty(),
            type_selects: TypeSelects::empty(),
            results: ResultsView::empty("Search", "No matches."),
            oob: true,
        };

        // Act
        let filtered = partial_with(form("failover", "postgresql", ""))
            .render()
            .expect("the filtered partial renders");
        let plain = partial_with(form("failover", "", ""))
            .render()
            .expect("the plain partial renders");

        // Assert: the toggle and its label swap out of band, so the count
        // chip tracks the filters applied through the htmx flow.
        assert!(filtered.contains(
            "id=\"pgokf-filters-toggle\" class=\"filters-state\" aria-controls=\"pgokf-filters-panel\" checked hx-swap-oob=\"true\""
        ));
        assert!(filtered.contains("id=\"pgokf-filters-toggle-label\""));
        assert!(filtered.contains("<span class=\"chip active\">1 active</span>"));
        assert!(plain.contains("aria-controls=\"pgokf-filters-panel\" hx-swap-oob=\"true\""));
        assert!(!plain.contains("checked"));
        assert!(!plain.contains("active</span>"));
    }

    fn hit_view(title: Option<&str>, ranked: bool) -> HitView {
        HitView {
            bundle_id: 2,
            bundle_name: "core".to_owned(),
            concept_id: "runbooks/failover".to_owned(),
            path: "runbooks/failover.md".to_owned(),
            title: title.map(str::to_owned),
            concept_type: Some("Runbook".to_owned()),
            rank: ranked.then_some(0.25),
            rank_display: ranked.then(|| "0.250".to_owned()),
            headline_html: Some("a <b>failover</b> runbook".to_owned()),
            tags: vec!["postgresql".to_owned()],
            href: "/concepts/2/runbooks%2Ffailover".to_owned(),
        }
    }

    #[test]
    fn results_partial_renders_hits_as_a_table_with_a_pager() {
        // Arrange: one ranked hit and one browsed (unranked) row, on a
        // continued page that also has a next page.
        let results = ResultsView {
            hits: vec![hit_view(Some("Failover runbook"), true), hit_view(None, false)],
            heading: "Results for \u{201c}failover\u{201d}".to_owned(),
            summary: "2 more.".to_owned(),
            notice: None,
            next_url: Some(
                "/search?q=failover&after_rank=0.25&after_bundle=2&after_id=runbooks%2Ffailover"
                    .to_owned(),
            ),
            next_partial_url: Some(
                "/search/results?q=failover&after_rank=0.25&after_bundle=2&after_id=runbooks%2Ffailover"
                    .to_owned(),
            ),
            prev_url: Some("/search?q=failover".to_owned()),
            browsing: false,
            degraded: false,
        };
        let partial = ResultsPartial {
            form: form("failover", "", ""),
            facets: FacetsView::empty(),
            type_selects: TypeSelects::empty(),
            results,
            oob: true,
        };

        // Act
        let rendered = partial.render().expect("the partial renders");

        // Assert: a real table - one header cell per column, one row per hit.
        assert!(rendered.contains("<table class=\"table hits-table\">"));
        for column in [
            "<th class=\"col-title\" scope=\"col\">Title</th>",
            "<th class=\"col-type\" scope=\"col\">Type</th>",
            "<th class=\"col-bundle\" scope=\"col\">Bundle</th>",
            "<th class=\"col-path\" scope=\"col\">Path</th>",
            "<th class=\"col-rank num\" scope=\"col\">Rank</th>",
            "<th class=\"col-tags\" scope=\"col\">Tags</th>",
            "<th class=\"col-snippet\" scope=\"col\">Snippet</th>",
        ] {
            assert!(rendered.contains(column), "missing column {column}");
        }
        assert!(rendered.matches("<tr class=\"hit\">").count() == 2);
        assert!(rendered.contains(
            "<a class=\"hit-title\" href=\"/concepts/2/runbooks%2Ffailover\" title=\"Failover runbook\">Failover runbook</a>"
        ));
        // The untitled hit falls back to its id, the unranked row shows no number.
        assert!(rendered.contains(">runbooks/failover</a>"));
        assert!(rendered.contains("title=\"Browse listings are not ranked\">—</span>"));
        assert!(
            rendered
                .contains("<td class=\"col-type\"><span class=\"pill type\">Runbook</span></td>")
        );
        assert!(rendered.contains("<a href=\"/bundles/2\">core</a>"));
        assert!(rendered.contains("title=\"runbooks/failover.md\">runbooks/failover.md</span>"));
        assert!(rendered.contains("<span class=\"tag\">postgresql</span>"));
        assert!(rendered.contains("a <b>failover</b> runbook"));
        // Each row carries the Details disclosure that expands the clamped
        // snippet on a wide screen and stands in for the hidden columns on
        // a phone.
        assert!(rendered.matches("<details class=\"hit-detail\">").count() == 2);

        // Assert: the pager is keyset-honest - a First-page link (no
        // fabricated numbers), a Next link that htmx-swaps the results
        // container with the partial URL, and no "load more" append.
        assert!(rendered.contains("<nav class=\"pager\" aria-label=\"Result pages\">"));
        assert!(
            rendered
                .contains("<a class=\"btn\" href=\"/search?q=failover\">&laquo; First page</a>")
        );
        assert!(rendered.contains("a later page, more follow"));
        assert!(rendered.contains("hx-get=\"/search/results?q=failover&#38;after_rank=0.25"));
        assert!(rendered.contains("hx-target=\"#pgokf-results\""));
        assert!(!rendered.contains("Load more"));
        assert!(!rendered.contains("beforeend"));
    }

    #[test]
    fn results_partial_hides_the_pager_on_a_single_first_page() {
        // Arrange: a first page with nothing after it.
        let partial = ResultsPartial {
            form: form("failover", "", ""),
            facets: FacetsView::empty(),
            type_selects: TypeSelects::empty(),
            results: ResultsView {
                hits: vec![hit_view(Some("Failover runbook"), true)],
                next_url: None,
                next_partial_url: None,
                prev_url: None,
                ..ResultsView::empty("Results", "1 match, ranked by lexical relevance.")
            },
            oob: true,
        };

        // Act
        let rendered = partial.render().expect("the partial renders");

        // Assert: the table renders but no pager chrome appears.
        assert!(rendered.contains("<table class=\"table hits-table\">"));
        assert!(!rendered.contains("class=\"pager\""));
    }

    #[test]
    fn search_page_renders_the_mobile_filter_toggle_with_the_active_count() {
        // Arrange: one page with a non-default filter, one without.
        let page_with = |form: SearchForm| SearchPage {
            shell: Shell {
                nav: "search".to_owned(),
                ..Shell::bare("Search")
            },
            form,
            bundles: Vec::new(),
            facets: FacetsView::empty(),
            type_selects: TypeSelects::empty(),
            results: ResultsView::empty("Search", "Type a query, or pick a filter to browse."),
            semantic_available: false,
            backend_label: "native".to_owned(),
        };

        // Act
        let filtered = page_with(form("failover", "", "2"))
            .render()
            .expect("the filtered page renders");
        let plain = page_with(form("", "", ""))
            .render()
            .expect("the plain page renders");

        // Assert: the toggle is checked with a count chip when a filter is
        // active, plain otherwise; the panel keeps its hook for the label.
        assert!(filtered.contains(
            "id=\"pgokf-filters-toggle\" class=\"filters-state\" aria-controls=\"pgokf-filters-panel\" checked"
        ));
        assert!(filtered.contains("<span class=\"chip active\">1 active</span>"));
        assert!(filtered.contains("id=\"pgokf-filters-panel\""));
        assert!(plain.contains("class=\"filters-state\""));
        assert!(!plain.contains("checked"));
        assert!(!plain.contains("active</span>"));

        // Assert: the current top nav tab carries aria-current (the script
        // scrolls it into the strip's view after each load).
        assert!(filtered.contains("href=\"/search\" class=\"active\" aria-current=\"page\""));
        assert_eq!(filtered.matches("aria-current=\"page\"").count(), 1);
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
    fn plugin_params_carry_the_stale_policy_and_closure() {
        // Arrange
        let params = PluginParams {
            stale_policy: "exclude".to_owned(),
            seeds: "1:runbooks/a\n2:b".to_owned(),
            relation_types: "docs:references, ops:depends".to_owned(),
            direction: "both".to_owned(),
            hops: "3".to_owned(),
            require_closure: "1".to_owned(),
            ..PluginParams::default()
        };

        // Act
        let (form, _, selection, _) = params.normalize().ok().expect("valid");

        // Assert
        assert_eq!(
            selection.stale_policy,
            pgokf_workspace::StalePolicy::Exclude
        );
        assert_eq!(
            selection
                .seeds
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>(),
            ["1:runbooks/a", "2:b"]
        );
        assert_eq!(selection.relation_types, ["docs:references", "ops:depends"]);
        assert_eq!(selection.direction, pgokf_workspace::Direction::Both);
        assert_eq!(selection.hops, Some(3));
        assert!(selection.require_closure);
        assert!(form.seed_problem.is_none());
        for part in [
            "stale_policy=exclude",
            "seeds=1%3Arunbooks%2Fa%0A2%3Ab",
            "direction=both",
            "hops=3",
            "require_closure=1",
        ] {
            assert!(
                form.query_string.contains(part),
                "{part} in {}",
                form.query_string
            );
        }

        // Bad values are refused; defaults stay the old behavior.
        for bad in [
            PluginParams {
                stale_policy: "drop".to_owned(),
                ..PluginParams::default()
            },
            PluginParams {
                direction: "up".to_owned(),
                ..PluginParams::default()
            },
            PluginParams {
                hops: "99".to_owned(),
                ..PluginParams::default()
            },
        ] {
            assert!(bad.normalize().is_err());
        }
        let (form, _, selection, _) = PluginParams::default().normalize().ok().expect("valid");
        assert_eq!(selection.stale_policy, pgokf_workspace::StalePolicy::Warn);
        assert!(selection.seeds.is_empty() && !selection.require_closure);
        assert!(!form.query_string.contains("stale_policy"));
        assert!(!form.query_string.contains("seeds"));

        // A malformed seeds line previews with a message and refuses the
        // download through the form's problems.
        let (form, _, selection, _) = PluginParams {
            seeds: "1:a\nnope".to_owned(),
            ..PluginParams::default()
        }
        .normalize()
        .ok()
        .expect("previews with the good seeds");
        assert_eq!(selection.seeds.len(), 1);
        assert!(form.problems().is_some(), "the download refuses it");
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
            relationships: vec![],
            total: 2,
        };

        // Act
        let value = graph_json(None, 0, &graph, ColorBy::Bundle, EdgeSource::Both);

        // Assert
        assert_eq!(value["nodes"][0]["id"], "2:a/b");
        assert_eq!(value["nodes"][1]["id"], "1:a/b");
        assert_eq!(value["nodes"][0]["title"], "a/b");
        assert_eq!(value["nodes"][0]["href"], "/concepts/2/a/b");
        assert_eq!(
            value["nodes"][0]["graph_href"],
            "/api/graph?seed=2%3Aa%2Fb&edges=both"
        );
        assert_eq!(value["links"][0]["source"], "2:a/b");
        assert_eq!(value["links"][0]["kind"], "link");
        assert_eq!(value["links"][0]["undirected"], false);
        assert_eq!(value["legend"], serde_json::json!(["docs", "sample"]));
        assert_eq!(value["color_by"], "bundle");
    }

    #[test]
    fn graph_json_labels_typed_edges_with_their_relation_types_and_direction() {
        // Arrange: a cross-bundle typed edge, folded over two rows, only one
        // of them undirected (so the fold keeps the arrow).
        let node = |bundle_id: i64, id: &str| crate::db::GraphNode {
            bundle_id,
            bundle_name: "docs".to_owned(),
            id: id.to_owned(),
            title: None,
            concept_type: None,
            path: format!("{id}.md"),
            hops: 0,
            degree: 1,
        };
        let graph = Graph {
            nodes: vec![node(1, "a"), node(2, "b")],
            links: vec![],
            relationships: vec![crate::db::GraphRelationship {
                source_bundle_id: 1,
                source: "a".to_owned(),
                target_bundle_id: 2,
                target: "b".to_owned(),
                count: 2,
                relations: vec![
                    crate::db::RelationType {
                        name: "tests:covers".to_owned(),
                        undirected: false,
                    },
                    crate::db::RelationType {
                        name: "tests:fixtures".to_owned(),
                        undirected: true,
                    },
                ],
                undirected: false,
            }],
            total: 2,
        };

        // Act
        let value = graph_json(None, 0, &graph, ColorBy::Bundle, EdgeSource::Both);

        // Assert: the edge joins the shared links array, labelled by kind,
        // keyed by its own per-end bundle ids, carrying the relation types
        // (opaque producer data, passed through verbatim) each with its own
        // direction, plus the fold's all-undirected flag the canvas arrow
        // follows.
        let edge = &value["links"][0];
        assert_eq!(edge["kind"], "relationship");
        assert_eq!(edge["source"], "1:a");
        assert_eq!(edge["target"], "2:b");
        assert_eq!(edge["count"], 2);
        assert_eq!(
            edge["relations"],
            serde_json::json!([
                {"name": "tests:covers", "undirected": false},
                {"name": "tests:fixtures", "undirected": true},
            ])
        );
        assert_eq!(edge["undirected"], false);
        assert_eq!(edge["texts"], serde_json::json!([]));
    }

    #[test]
    fn graph_json_explore_urls_carry_the_pictures_edge_source() {
        // Arrange
        let node = |bundle_id: i64, id: &str| crate::db::GraphNode {
            bundle_id,
            bundle_name: "docs".to_owned(),
            id: id.to_owned(),
            title: None,
            concept_type: None,
            path: format!("{id}.md"),
            hops: 0,
            degree: 1,
        };
        let graph = Graph {
            nodes: vec![node(2, "runbooks/a")],
            links: vec![],
            relationships: vec![],
            total: 1,
        };

        // Act & Assert: the per-node URL both Explore actions load points at
        // the source-aware seeded endpoint with the picture's own edge
        // source, never at the concept endpoint's both-sources default.
        for (edges, value) in [
            (EdgeSource::Links, "links"),
            (EdgeSource::Relationships, "rels"),
            (EdgeSource::Both, "both"),
        ] {
            let json = graph_json(None, 0, &graph, ColorBy::Bundle, edges);
            assert_eq!(
                json["nodes"][0]["graph_href"],
                format!("/api/graph?seed=2%3Arunbooks%2Fa&edges={value}")
            );
        }
        // The receiving endpoint parses that seed and source pair back.
        let params = CatalogGraphParams::parse(Some("seed=2%3Arunbooks%2Fa&edges=links"));
        assert_eq!(
            params.seed().ok().flatten(),
            Some((2, "runbooks/a".to_owned()))
        );
        assert_eq!(params.edge_source().ok(), Some(EdgeSource::Links));
    }

    #[test]
    fn graph_js_both_explore_actions_load_the_nodes_source_aware_url() {
        // Assert: the node card's "Explore from here" and the edge card's
        // "Explore" buttons both carry the node's graph_href (which embeds
        // the edge source), and the shared click handler seeds the loader
        // with it, so exploring never silently resets the edge source.
        let node_button = "data-explore=\"' + escapeHtml(node.graph_href)";
        let edge_button = "data-explore=\"' + escapeHtml(to.graph_href)";
        assert!(
            GRAPH_JS.contains(node_button),
            "the node card's Explore action carries graph_href"
        );
        assert!(
            GRAPH_JS.contains(edge_button),
            "the edge card's Explore action carries graph_href"
        );
        assert!(
            GRAPH_JS.contains("seedUrl = explore.getAttribute('data-explore');"),
            "the explore handler loads the action's URL as the new seed"
        );
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
    fn catalog_graph_params_parse_the_edge_source_with_both_as_the_default() {
        // Arrange & Act & Assert
        assert_eq!(
            CatalogGraphParams::parse(None).edge_source().ok(),
            Some(EdgeSource::Both)
        );
        assert_eq!(
            CatalogGraphParams::parse(Some("edges=")).edge_source().ok(),
            Some(EdgeSource::Both)
        );
        assert_eq!(
            CatalogGraphParams::parse(Some("edges=both"))
                .edge_source()
                .ok(),
            Some(EdgeSource::Both)
        );
        assert_eq!(
            CatalogGraphParams::parse(Some("edges=links"))
                .edge_source()
                .ok(),
            Some(EdgeSource::Links)
        );
        assert_eq!(
            CatalogGraphParams::parse(Some("edges=rels"))
                .edge_source()
                .ok(),
            Some(EdgeSource::Relationships)
        );
        // An unknown value is rejected, not silently widened to the default.
        assert!(
            CatalogGraphParams::parse(Some("edges=all"))
                .edge_source()
                .is_err()
        );
        // The value percent-decodes like every other key and value.
        assert_eq!(
            CatalogGraphParams::parse(Some("%65dges=links"))
                .edge_source()
                .ok(),
            Some(EdgeSource::Links)
        );
    }

    #[test]
    fn edge_source_round_trips_through_its_query_value() {
        // Arrange & Act & Assert
        for source in [
            EdgeSource::Links,
            EdgeSource::Relationships,
            EdgeSource::Both,
        ] {
            let params = CatalogGraphParams::parse(Some(&format!("edges={}", source.as_str())));
            assert_eq!(params.edge_source().ok(), Some(source));
        }
    }

    #[test]
    fn catalog_graph_params_collect_repeated_bundles_and_keep_the_legacy_form() {
        // Arrange & Act
        let multi = CatalogGraphParams::parse(Some("bundle=2&bundle=5&limit=600"));
        let legacy = CatalogGraphParams::parse(Some("bundle=5"));
        let blank = CatalogGraphParams::parse(Some("bundle=&limit=100"));
        let absent = CatalogGraphParams::parse(None);

        // Assert
        assert_eq!(multi.bundle_ids().ok().expect("valid"), vec![2, 5]);
        assert_eq!(multi.limit(), 600);
        assert_eq!(legacy.bundle_ids().ok().expect("valid"), vec![5]);
        assert!(blank.bundle_ids().ok().expect("valid").is_empty());
        assert!(absent.bundle_ids().ok().expect("valid").is_empty());
    }

    #[test]
    fn catalog_graph_params_reject_a_non_integer_bundle() {
        // Arrange & Act & Assert
        assert!(
            CatalogGraphParams::parse(Some("bundle=abc"))
                .bundle_ids()
                .is_err()
        );
        assert!(
            CatalogGraphParams::parse(Some("bundle=2&bundle=x"))
                .bundle_ids()
                .is_err()
        );
    }

    #[test]
    fn catalog_graph_params_decode_encoded_keys() {
        // Arrange & Act
        let encoded = CatalogGraphParams::parse(Some("%62undle=5"));
        let mixed = CatalogGraphParams::parse(Some("bundle=2&%62undle=5"));
        let scalars =
            CatalogGraphParams::parse(Some("%73eed=2%3Arunbooks%2Fa&%6Cimit=600&%68ops=3"));

        // Assert
        assert_eq!(encoded.bundle_ids().ok().expect("valid"), vec![5]);
        assert_eq!(mixed.bundle_ids().ok().expect("valid"), vec![2, 5]);
        assert_eq!(
            scalars.seed().ok().flatten(),
            Some((2, "runbooks/a".to_owned()))
        );
        assert_eq!(scalars.limit(), 600);
        assert_eq!(parse_hops(&scalars.hops), 3);
    }

    #[test]
    fn catalog_graph_params_dedupe_repeated_bundle_ids() {
        // Arrange & Act
        let params = CatalogGraphParams::parse(Some("bundle=5&bundle=5&bundle=2&bundle=5"));

        // Assert
        assert_eq!(params.bundle_ids().ok().expect("valid"), vec![5, 2]);
        assert_eq!(
            CatalogGraphParams::parse(Some("bundle=5&bundle=5"))
                .bundle_ids()
                .ok()
                .expect("valid"),
            vec![5]
        );
    }

    #[test]
    fn catalog_graph_params_trim_whitespace_around_bundle_ids() {
        // Arrange & Act
        let padded = CatalogGraphParams::parse(Some("bundle=%205%20"));
        let blank = CatalogGraphParams::parse(Some("bundle=%20%20"));

        // Assert
        assert_eq!(padded.bundle_ids().ok().expect("valid"), vec![5]);
        assert!(blank.bundle_ids().ok().expect("valid").is_empty());
    }

    #[test]
    fn catalog_graph_params_decode_form_encoding() {
        // Arrange & Act
        let params = CatalogGraphParams::parse(Some("seed=2%3Arunbooks%2Fa+b&hops=3"));

        // Assert
        assert_eq!(
            params.seed().ok().flatten(),
            Some((2, "runbooks/a b".to_owned()))
        );
        assert_eq!(parse_hops(&params.hops), 3);
    }

    #[test]
    fn catalog_graph_params_collect_repeated_types_and_decode_them() {
        // Arrange & Act
        let multi = CatalogGraphParams::parse(Some(
            "type_group=code&type=Guide&type_group=code&type=group%3Acode",
        ));
        let blank = CatalogGraphParams::parse(Some("type=&limit=100"));
        let absent = CatalogGraphParams::parse(Some("bundle=2"));

        // Assert: percent-decoded, duplicates collapsed, blanks ignored;
        // group slugs collect under their own parameter, and a legacy
        // `group:`-prefixed exact type stays a verbatim `type` value.
        assert_eq!(
            multi.type_values(),
            vec!["Guide".to_owned(), "group:code".to_owned()]
        );
        assert_eq!(multi.group_slugs(), vec!["code".to_owned()]);
        assert!(blank.type_values().is_empty());
        assert!(absent.type_values().is_empty());
        assert!(absent.group_slugs().is_empty());
    }

    #[test]
    fn expand_graph_types_passes_exact_types_through_without_the_catalog() {
        // Arrange: exact values only, including a legacy `group:`-prefixed
        // type string, which now matches verbatim.
        let exact = vec!["Guide".to_owned(), "group:code".to_owned()];

        // Act
        let (types, impossible) = expand_graph_types(&exact, &[], &[])
            .ok()
            .expect("valid groups");

        // Assert
        assert_eq!(types, vec!["Guide".to_owned(), "group:code".to_owned()]);
        assert!(!impossible);
    }

    #[test]
    fn expand_graph_types_expands_groups_against_the_observed_types() {
        // Arrange: mixed group and exact selections, with a type no known
        // group claims.
        let observed: Vec<String> = vec![
            "Code Entity".to_owned(),
            "Guide".to_owned(),
            "Qualia".to_owned(),
        ];
        let exact = vec!["Guide".to_owned()];
        let groups = vec!["code".to_owned(), "other".to_owned()];

        // Act
        let (types, impossible) = expand_graph_types(&exact, &groups, &observed)
            .ok()
            .expect("valid groups");

        // Assert: group members come from the observed types, the unknown
        // type is reachable through "Other", duplicates collapse.
        assert_eq!(
            types,
            vec![
                "Guide".to_owned(),
                "Code Entity".to_owned(),
                "Qualia".to_owned()
            ]
        );
        assert!(!impossible);
    }

    #[test]
    fn expand_graph_types_flags_a_group_with_no_observed_members() {
        // Arrange
        let groups = vec!["code".to_owned()];
        let observed: Vec<String> = vec!["Guide".to_owned()];

        // Act
        let (types, impossible) = expand_graph_types(&[], &groups, &observed)
            .ok()
            .expect("valid groups");

        // Assert: filters were given but match nothing - never unfiltered.
        assert!(types.is_empty());
        assert!(impossible);
        // No filters at all is not impossible.
        let (types, impossible) = expand_graph_types(&[], &[], &observed).ok().expect("valid");
        assert!(types.is_empty());
        assert!(!impossible);
    }

    #[test]
    fn expand_graph_types_rejects_an_unknown_group_slug() {
        // Arrange
        let groups = vec!["bogus".to_owned()];

        // Act & Assert
        assert!(expand_graph_types(&[], &groups, &[]).is_err());
    }

    #[test]
    fn graph_type_filters_lists_observed_groups_and_keeps_selected_ones() {
        // Arrange
        let observed = vec![
            Facet {
                value: "Code Entity".to_owned(),
                count: 12_006,
            },
            Facet {
                value: "Guide".to_owned(),
                count: 10,
            },
            Facet {
                value: "Qualia".to_owned(),
                count: 3,
            },
        ];
        let selected_types = vec!["Widget".to_owned()];
        let selected_groups = vec!["documents".to_owned()];

        // Act
        let filters = graph_type_filters(&observed, &selected_types, &selected_groups);

        // Assert: every observed group renders (unknown types under Other),
        // the selected group is checked, and a selected exact type no group
        // covers still shows, checked.
        let values: Vec<(&str, &str, bool, bool)> = filters
            .iter()
            .map(|f| (f.value.as_str(), f.label.as_str(), f.selected, f.group))
            .collect();
        assert_eq!(
            values,
            vec![
                ("code", "Code", false, true),
                ("documents", "Documents", true, true),
                ("other", "Other", false, true),
                ("Widget", "Widget", true, false),
            ]
        );
    }

    #[test]
    fn graph_type_filters_keeps_a_selected_group_with_no_observed_members() {
        // Arrange: the catalog observed only Guide, but the URL selects the
        // Code group (whose expansion is empty and matches nothing).
        let observed = vec![Facet {
            value: "Guide".to_owned(),
            count: 10,
        }];
        let selected_groups = vec!["code".to_owned()];

        // Act
        let filters = graph_type_filters(&observed, &[], &selected_groups);

        // Assert: the Code control still renders, checked, so Draw
        // resubmits the filter instead of silently dropping it.
        let values: Vec<(&str, &str, bool, bool)> = filters
            .iter()
            .map(|f| (f.value.as_str(), f.label.as_str(), f.selected, f.group))
            .collect();
        assert_eq!(
            values,
            vec![
                ("documents", "Documents", false, true),
                ("code", "Code", true, true),
            ]
        );
    }

    #[test]
    fn graph_url_query_repeats_the_bundle_and_type_params_per_selection() {
        // Arrange & Act & Assert
        assert_eq!(
            graph_url_query(&[2, 5], &[], &[], 300, None, EdgeSource::Both),
            "bundle=2&bundle=5&limit=300&edges=both"
        );
        assert_eq!(
            graph_url_query(&[], &[], &[], 100, None, EdgeSource::Both),
            "limit=100&edges=both"
        );
        assert_eq!(
            graph_url_query(
                &[5],
                &[],
                &[],
                300,
                Some(&(2, "runbooks/a".to_owned())),
                EdgeSource::Both,
            ),
            "bundle=5&limit=300&edges=both&seed=2%3Arunbooks%2Fa"
        );
        assert_eq!(
            graph_url_query(
                &[],
                &["Guide".to_owned()],
                &["code".to_owned()],
                300,
                None,
                EdgeSource::Both,
            ),
            "type=Guide&type_group=code&limit=300&edges=both"
        );
    }

    #[test]
    fn graph_url_query_round_trips_the_edge_source() {
        // Arrange & Act & Assert: every edge source reaches the API verbatim.
        assert_eq!(
            graph_url_query(&[], &[], &[], 300, None, EdgeSource::Links),
            "limit=300&edges=links"
        );
        assert_eq!(
            graph_url_query(&[], &[], &[], 300, None, EdgeSource::Relationships),
            "limit=300&edges=rels"
        );
    }

    #[test]
    fn graph_page_marks_every_selected_bundle_in_the_checkbox_list() {
        // Arrange
        let bundle = |id: i64, name: &str| BundleInfo {
            id,
            path: name.to_owned(),
            name: name.to_owned(),
            okf_version: None,
            file_count: 0,
            last_synced_at: None,
            enabled: true,
        };
        let page = GraphPage {
            shell: Shell::bare("Graph"),
            bundles: vec![bundle(1, "alpha"), bundle(2, "docs"), bundle(5, "wiki")],
            bundle_ids: vec![2, 5],
            type_filters: Vec::new(),
            limit_options: vec![(300, true)],
            edge_options: vec![
                ("both", true, "Links and relationships"),
                ("links", false, "Document links only"),
                ("rels", false, "Relationships only"),
            ],
            graph_url: "/api/graph?bundle=2&bundle=5&limit=300&edges=both".to_owned(),
            catalog_url: "/graph?bundle=2&bundle=5&limit=300&edges=both".to_owned(),
            hops: 2,
            seed_label: None,
        };

        // Act
        let rendered = page.render().expect("the graph page renders");

        // Assert
        // Checked boxes submit the same repeated `bundle` params the multi-select did.
        assert!(
            rendered.contains("<input type=\"checkbox\" name=\"bundle\" value=\"2\" checked> docs")
        );
        assert!(
            rendered.contains("<input type=\"checkbox\" name=\"bundle\" value=\"5\" checked> wiki")
        );
        assert!(!rendered.contains("value=\"1\" checked"));
        assert!(rendered.contains("No selection = all bundles"));
        // The filter, count, and clear affordances carry the hooks graph.js wires up.
        assert!(rendered.contains("data-bundle-filter=\"#g-bundle-list\""));
        assert!(rendered.contains("class=\"filter-input js-only\""));
        assert!(rendered.contains("id=\"g-bundle-list\" data-bundle-list"));
        assert!(rendered.contains("<li data-filter-text=\"docs\">"));
        assert!(rendered.contains("data-bundle-empty"));
        assert!(rendered.contains("data-bundle-count"));
        assert!(rendered.contains("aria-live=\"polite\""));
        assert!(rendered.contains("data-bundle-clear"));
        assert!(rendered.contains("data-bundle-visible"));
        // The drawn endpoint carries every selected bundle (HTML-escaped).
        assert!(rendered.contains("/api/graph?bundle=2&#38;bundle=5&#38;limit=300&#38;edges=both"));
    }

    #[test]
    fn graph_page_renders_the_edge_source_select_with_the_current_choice() {
        // Arrange
        let page = GraphPage {
            shell: Shell::bare("Graph"),
            bundles: Vec::new(),
            bundle_ids: Vec::new(),
            type_filters: Vec::new(),
            limit_options: vec![(300, true)],
            edge_options: vec![
                ("both", false, "Links and relationships"),
                ("links", true, "Document links only"),
                ("rels", false, "Relationships only"),
            ],
            graph_url: "/api/graph?limit=300&edges=links".to_owned(),
            catalog_url: "/graph?limit=300&edges=links".to_owned(),
            hops: 2,
            seed_label: None,
        };

        // Act
        let rendered = page.render().expect("the graph page renders");

        // Assert: the select submits `edges` like the limit select submits
        // `limit`, the current choice is preselected, and the generic
        // wording carries no producer vocabulary.
        assert!(rendered.contains("<select id=\"g-edges\" name=\"edges\">"));
        assert!(rendered.contains("<option value=\"links\" selected>Document links only</option>"));
        assert!(rendered.contains("<option value=\"both\" >Links and relationships</option>"));
        assert!(rendered.contains("<option value=\"rels\" >Relationships only</option>"));
        assert!(rendered.contains("data-graph-legend"));
    }

    #[test]
    fn graph_page_marks_the_selected_type_groups_in_the_checkbox_list() {
        // Arrange
        let filter = |value: &str, label: &str, selected: bool, group: bool| GraphTypeFilter {
            value: value.to_owned(),
            label: label.to_owned(),
            selected,
            group,
        };
        let page = GraphPage {
            shell: Shell::bare("Graph"),
            bundles: Vec::new(),
            bundle_ids: Vec::new(),
            type_filters: vec![
                filter("code", "Code", true, true),
                filter("documents", "Documents", false, true),
                filter("other", "Other", false, true),
                filter("Widget", "Widget", true, false),
            ],
            limit_options: vec![(300, true)],
            edge_options: vec![("both", true, "Links and relationships")],
            graph_url: "/api/graph?type=Widget&type_group=code&limit=300&edges=both".to_owned(),
            catalog_url: "/graph?type=Widget&type_group=code&limit=300&edges=both".to_owned(),
            hops: 2,
            seed_label: None,
        };

        // Act
        let rendered = page.render().expect("the graph page renders");

        // Assert: the group list mirrors the bundle picker's pattern - the
        // checked group submits its slug under the separate `type_group`
        // parameter, a selected exact type submits the legacy `type`
        // parameter verbatim, and the hint and clear affordance match.
        assert!(
            rendered.contains(
                "<input type=\"checkbox\" name=\"type_group\" value=\"code\" checked> Code"
            )
        );
        assert!(rendered.contains("name=\"type_group\" value=\"documents\"> Documents"));
        assert!(rendered.contains("name=\"type_group\" value=\"other\"> Other"));
        assert!(
            rendered.contains(
                "<input type=\"checkbox\" name=\"type\" value=\"Widget\" checked> Widget"
            )
        );
        assert!(rendered.contains("No selection = all types"));
        assert!(rendered.contains("data-type-clear"));
        assert!(rendered.contains("aria-describedby=\"g-type-hint\""));
        // The drawn endpoint carries the selections (HTML-escaped).
        assert!(
            rendered.contains(
                "/api/graph?type=Widget&#38;type_group=code&#38;limit=300&#38;edges=both"
            )
        );
    }

    #[test]
    fn graph_page_with_no_selection_renders_the_checkbox_list_unchecked() {
        // Arrange
        let page = GraphPage {
            shell: Shell::bare("Graph"),
            bundles: Vec::new(),
            bundle_ids: Vec::new(),
            type_filters: Vec::new(),
            limit_options: vec![(300, false)],
            edge_options: vec![("both", true, "Links and relationships")],
            graph_url: "/api/graph?limit=300&edges=both".to_owned(),
            catalog_url: "/graph?limit=300&edges=both".to_owned(),
            hops: 2,
            seed_label: None,
        };

        // Act
        let rendered = page.render().expect("the graph page renders");

        // Assert
        assert!(rendered.contains("data-bundle-list"));
        assert!(!rendered.contains("checked"));
        // With no observed or selected types the group list stays hidden.
        assert!(!rendered.contains("g-type-list"));
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

    #[test]
    fn resolve_cadence_maps_the_presets_to_fixed_cron_expressions() {
        // Arrange & Act & Assert
        assert_eq!(resolve_cadence("off", "").ok().flatten(), None);
        assert_eq!(resolve_cadence("", "").ok().flatten(), None);
        assert_eq!(
            resolve_cadence("15min", "").ok().flatten().as_deref(),
            Some("*/15 * * * *")
        );
        assert_eq!(
            resolve_cadence("hourly", "").ok().flatten().as_deref(),
            Some("0 * * * *")
        );
        assert_eq!(
            resolve_cadence("6h", "").ok().flatten().as_deref(),
            Some("0 */6 * * *")
        );
        assert_eq!(
            resolve_cadence("daily", "").ok().flatten().as_deref(),
            Some("0 3 * * *")
        );
        // A custom value goes through validation; the field is ignored for
        // presets and off.
        assert_eq!(
            resolve_cadence("custom", "*/30 * * * *")
                .ok()
                .flatten()
                .as_deref(),
            Some("*/30 * * * *")
        );
        assert_eq!(
            resolve_cadence("hourly", "not a cron")
                .ok()
                .flatten()
                .as_deref(),
            Some("0 * * * *")
        );
        assert!(resolve_cadence("weekly", "").is_err(), "an unknown preset");
    }

    #[test]
    fn cadence_round_trips_through_set_and_read_back() {
        // Arrange & Act & Assert: every preset the form can set humanizes
        // back to that same preset when the schedule is read again, and off
        // reads back as Off.
        for (value, label, cron) in CADENCE_PRESETS {
            let schedule = resolve_cadence(value, "").ok().flatten();
            assert_eq!(schedule.as_deref(), Some(*cron));
            let view = cadence_view(schedule.as_deref());
            assert_eq!(view.preset, *value);
            assert_eq!(view.label, *label);
        }
        let off = resolve_cadence("off", "").ok().flatten();
        assert_eq!(off, None);
        let view = cadence_view(off.as_deref());
        assert_eq!(view.preset, "off");
        assert_eq!(view.label, "Off");
        // A schedule no preset owns reads back as custom, verbatim.
        let custom = cadence_view(Some("17 4 * * 1-5"));
        assert_eq!(custom.preset, "custom");
        assert_eq!(custom.label, "17 4 * * 1-5");
        assert_eq!(custom.custom, "17 4 * * 1-5");
    }

    #[test]
    fn validate_cron_schedule_accepts_cron_expressions_and_interval_phrases() {
        // Arrange & Act & Assert
        for good in [
            "* * * * *",
            "*/15 * * * *",
            "0 */6 * * *",
            "0 9 * * MON-FRI",
            "30 2 1 jan *",
            "0,30 * * * *",
            "0 9-17/2 * * *",
            "  0 * * * *  ",
            "30 minutes",
            "1 hour",
            "45 seconds",
        ] {
            assert!(
                validate_cron_schedule(good).is_ok(),
                "{good:?} should validate"
            );
        }
        // Trimming is part of the contract.
        assert_eq!(
            validate_cron_schedule("  0 * * * *  ").ok().as_deref(),
            Some("0 * * * *")
        );
    }

    #[test]
    fn interval_phrases_translate_to_cron_expressions_before_they_reach_pg_cron() {
        // Arrange & Act & Assert: minute and hour phrases never reach the
        // scheduler as phrases (its interval parser accepts only 1-59
        // seconds); they become the equivalent 5-field cron expression,
        // while a seconds phrase passes through as pg_cron's own syntax.
        assert_eq!(
            validate_cron_schedule("45 seconds").ok().as_deref(),
            Some("45 seconds")
        );
        assert_eq!(
            validate_cron_schedule("1 second").ok().as_deref(),
            Some("1 seconds")
        );
        assert_eq!(
            validate_cron_schedule("30 minutes").ok().as_deref(),
            Some("*/30 * * * *")
        );
        assert_eq!(
            validate_cron_schedule("1 minute").ok().as_deref(),
            Some("*/1 * * * *")
        );
        assert_eq!(
            validate_cron_schedule("1 hour").ok().as_deref(),
            Some("0 * * * *")
        );
        assert_eq!(
            validate_cron_schedule("6 hours").ok().as_deref(),
            Some("0 */6 * * *")
        );
        assert_eq!(
            validate_cron_schedule("24 hours").ok().as_deref(),
            Some("0 0 * * *")
        );
    }

    #[test]
    fn validate_cron_schedule_rejects_malformed_input() {
        // Arrange
        let oversized = "1 ".repeat(100);

        // Act & Assert: everything here is refused before any database call.
        for bad_input in [
            "",
            "   ",
            "* * * *",
            "* * * * * *",
            "61 * * * *",
            "* 25 * * *",
            "* * 0 * *",
            "* * * 13 *",
            "* * * * 8",
            "* * * FOO *",
            "*/0 * * * *",
            "*/x * * * *",
            "0 * * * *; DROP TABLE cron.job",
            "0 minutes later",
            "0 weeks",
            // Phrase-shaped input the scheduler could not honor as entered:
            // pg_cron's interval parser takes 1-59 seconds only, and a
            // minute/hour count that no 5-field expression expresses is
            // refused rather than sent.
            "0 seconds",
            "60 seconds",
            "90 minutes",
            "5 hours",
            "25 hours",
        ] {
            assert!(
                validate_cron_schedule(bad_input).is_err(),
                "{bad_input:?} must be refused"
            );
        }
        assert!(
            validate_cron_schedule(&oversized).is_err(),
            "over 128 bytes"
        );
        assert!(validate_cron_schedule("0 * * *\0*").is_err(), "a NUL byte");
    }

    #[test]
    fn the_admin_gate_rejects_everything_but_the_admin_role() {
        // Arrange
        let session_with = |role: Role| Session {
            principal: Some(Principal {
                subject: "person".to_owned(),
                display: "Person".to_owned(),
                role,
            }),
            mode: Mode::Users,
            peer: None,
        };

        // Act & Assert
        assert!(admin(&session_with(Role::Admin)).is_ok());
        for role in [Role::Viewer, Role::Uploader, Role::Editor, Role::Approver] {
            let error = admin(&session_with(role)).expect_err("a non-admin is refused");
            assert_eq!(error.status(), StatusCode::FORBIDDEN);
        }
        // Nobody signed in is sent to sign-in, never to the page.
        let anon = admin(&Session::anonymous(Mode::Users)).expect_err("anonymous is refused");
        assert_eq!(anon.status(), StatusCode::SEE_OTHER);
    }

    /// An App with dead pools, for tests of gates that run before any
    /// statement is issued.
    fn test_app(writer: Option<Db>) -> App {
        App {
            db: crate::db::dead_db(),
            writer,
            mcp_tokens: None,
            auth: Authenticator::Anonymous,
            trusted_proxies: crate::auth::TrustedProxies::default(),
            rebuilds: tokio::sync::Mutex::new(()),
            builds: tokio::sync::Semaphore::new(MAX_PLUGIN_BUILDS),
            stores: crate::store::Stores::default(),
            embedder: None,
            producer: None,
            catalog_name: "test".to_owned(),
            tenant: None,
            version: "0".to_owned(),
            registry_visible: None,
            registry_rows: None,
        }
    }

    #[test]
    fn cadence_actions_need_the_admin_role_and_a_writer_connection() {
        // Arrange
        let admin_session = Session {
            principal: Some(Principal {
                subject: "operator".to_owned(),
                display: "Operator".to_owned(),
                role: Role::Admin,
            }),
            mode: Mode::Users,
            peer: None,
        };

        // Act & Assert: without a writer connection the action is
        // unavailable; with one, an admin passes and an editor is refused.
        let no_writer = test_app(None);
        let error = require(&no_writer, &admin_session, Role::Admin, "/admin")
            .err()
            .expect("no writer connection");
        assert_eq!(error.status(), StatusCode::SERVICE_UNAVAILABLE);
        let with_writer = test_app(Some(crate::db::dead_db()));
        assert!(require(&with_writer, &admin_session, Role::Admin, "/admin").is_ok());
        let editor = Session {
            principal: Some(Principal {
                subject: "editor".to_owned(),
                display: "Editor".to_owned(),
                role: Role::Editor,
            }),
            ..admin_session.clone()
        };
        let error = require(&with_writer, &editor, Role::Admin, "/admin")
            .err()
            .expect("a non-admin is refused");
        assert_eq!(error.status(), StatusCode::FORBIDDEN);
    }

    fn admin_bundle(id: i64, name: &str, freshness: Option<&str>) -> AdminBundle {
        AdminBundle {
            id,
            path: format!("/bundles/{name}"),
            name: name.to_owned(),
            source_type: "filesystem".to_owned(),
            enabled: true,
            retired: false,
            file_count: 3,
            last_synced_at: None,
            freshness: freshness.map(str::to_owned),
        }
    }

    #[test]
    fn admin_bundles_page_renders_freshness_and_the_cadence_form() {
        // Arrange: one bundle per cadence shape - a preset, off, and a
        // schedule no preset owns - plus one freshness state each.
        let page = AdminBundlesPage {
            shell: Shell::bare("Administration · Bundles"),
            admin: AdminTab::Bundles.shell(AdminOutcome::default()),
            bundles: vec![
                AdminBundleRow {
                    bundle: admin_bundle(7, "docs", Some("fresh")),
                    cadence: cadence_view(Some("0 * * * *")),
                },
                AdminBundleRow {
                    bundle: admin_bundle(8, "wiki", Some("stale")),
                    cadence: cadence_view(None),
                },
                AdminBundleRow {
                    bundle: admin_bundle(9, "notes", None),
                    cadence: cadence_view(Some("17 4 * * 1-5")),
                },
            ],
            cadence_known: true,
            cadence_note: None,
        };

        // Act
        let rendered = page.render().expect("the admin bundles page renders");

        // Assert: the new columns, the per-row cadence form with the current
        // state preselected, and the honest framing (a refresh schedule is
        // not a freshness attestation).
        assert!(rendered.contains("<th>Freshness</th>"));
        assert!(rendered.contains("<th>Refresh cadence</th>"));
        assert!(rendered.contains("<span class=\"pill ok\">fresh</span>"));
        assert!(rendered.contains("<span class=\"pill warn\">stale</span>"));
        assert!(rendered.contains("refreshing content does not attest freshness"));
        assert!(rendered.contains("<option value=\"hourly\" selected>Hourly</option>"));
        assert!(rendered.contains("<option value=\"off\" selected>Off</option>"));
        assert!(rendered.contains("<option value=\"custom\" selected>Custom"));
        assert!(rendered.contains("value=\"17 4 * * 1-5\""));
        assert!(rendered.contains("Now: hourly"));
        assert!(rendered.contains("Now: Off"));
        assert!(rendered.contains("Now: 17 4 * * 1-5"));
        assert!(rendered.contains("name=\"cadence\" data-cadence"));
        assert!(rendered.contains("name=\"action\" value=\"cadence\""));
        // Scheduling surfaces here only: no freshness-attesting action.
        assert!(!rendered.contains("certify"));
    }

    #[test]
    fn admin_bundles_page_degrades_when_the_schedule_read_is_impossible() {
        // Arrange
        let page = AdminBundlesPage {
            shell: Shell::bare("Administration · Bundles"),
            admin: AdminTab::Bundles.shell(AdminOutcome::default()),
            bundles: vec![AdminBundleRow {
                bundle: admin_bundle(7, "docs", Some("fresh")),
                cadence: cadence_view(None),
            }],
            cadence_known: false,
            cadence_note: Some(
                "pg_cron is not installed in this database, so no refresh cadence can be read or set here."
                    .to_owned(),
            ),
        };

        // Act
        let rendered = page.render().expect("the admin bundles page renders");

        // Assert: the note explains, the cells show the unknown marker, and
        // no cadence form is offered.
        assert!(rendered.contains("pg_cron is not installed in this database"));
        assert!(rendered.contains("&mdash;"));
        assert!(!rendered.contains("name=\"cadence\""));
        assert!(rendered.contains("<th>Refresh cadence</th>"));
    }

    #[test]
    fn admin_bundles_page_offers_refresh_for_content_bundles_too() {
        // Arrange: one filesystem bundle and one content bundle.
        let mut content = admin_bundle(10, "notes", None);
        content.source_type = "content".to_owned();
        let page = AdminBundlesPage {
            shell: Shell::bare("Administration · Bundles"),
            admin: AdminTab::Bundles.shell(AdminOutcome::default()),
            bundles: vec![
                AdminBundleRow {
                    bundle: admin_bundle(7, "docs", Some("fresh")),
                    cadence: cadence_view(None),
                },
                AdminBundleRow {
                    bundle: content,
                    cadence: cadence_view(None),
                },
            ],
            cadence_known: true,
            cadence_note: None,
        };

        // Act
        let rendered = page.render().expect("the admin bundles page renders");

        // Assert: Refresh is offered for both, each button's title saying
        // what it does - a content bundle has no source to re-read, so its
        // refresh rebuilds it from the sources the catalog keeps.
        assert_eq!(rendered.matches("value=\"refresh\"").count(), 2);
        assert!(rendered.contains("title=\"Re-read the source and index what changed\""));
        assert!(
            rendered.contains("title=\"Rebuild this bundle from the sources the catalog keeps\"")
        );
        // The header note keeps the freshness truth: a content rebuild
        // re-syncs content; the fresh label still asks for certification.
        assert!(rendered.contains("refreshing content does not attest freshness"));
        assert!(rendered.contains("the fresh label still asks for certification"));
    }

    // ---- Content-bundle refresh (scratch database) ------------------------

    /// The scratch database the content-refresh regression creates and
    /// drops. Like the scratch tests in `db.rs`, everything skips when no
    /// local `PostgreSQL` answers.
    const REFRESH_SCRATCH_DB: &str = "pgokf_web_refresh_sql_test";

    /// The pgokf stand-in fixture: the catalog tables the refresh flow
    /// reads, a `store_source` switch, and stand-ins for the two writer
    /// functions that record which one ran. The `register_bundle_content`
    /// stand-in also records whether the bundle's cross-writer advisory
    /// lock was held when it ran: a session-level lock another session
    /// holds cannot be taken, so a successful try means the refresh ran
    /// WITHOUT the lock the concurrency contract requires.
    const REFRESH_SCRATCH_FIXTURES: &str = "
        CREATE SCHEMA pgokf;
        CREATE TABLE pgokf.fixture (store_source boolean NOT NULL);
        INSERT INTO pgokf.fixture VALUES (true);
        CREATE TABLE pgokf.bundles (
            id bigint PRIMARY KEY,
            tenant_id text NOT NULL DEFAULT 'default',
            name text,
            path text NOT NULL,
            source_type text NOT NULL,
            enabled boolean NOT NULL DEFAULT true,
            retired_at timestamptz,
            file_count integer NOT NULL DEFAULT 0,
            okf_version text
        );
        CREATE TABLE pgokf.concepts (bundle_id bigint, id text, path text);
        CREATE TABLE pgokf.concept_source (bundle_id bigint, concept_id text, raw_content bytea);
        CREATE TABLE pgokf.skills (bundle_id bigint, concept_id text, skill_md bytea);
        CREATE TABLE pgokf.scripts (bundle_id bigint, concept_id text, exact_bytes bytea);
        CREATE TABLE pgokf.reference_documents (bundle_id bigint, concept_id text, exact_bytes bytea);
        CREATE TABLE pgokf.bundle_log (bundle_id bigint, note text);
        CREATE TABLE pgokf.calls (fn text, detail text);
        INSERT INTO pgokf.bundles
            (id, tenant_id, name, path, source_type, enabled, retired_at, file_count, okf_version)
        VALUES
            (11, 'default', 'notes', 'content:notes', 'content', true, NULL, 2, NULL),
            (12, 'default', 'docs', '/bundles/docs', 'filesystem', true, NULL, 1, NULL);
        INSERT INTO pgokf.concepts VALUES (11, 'a', 'a.md'), (11, 'b', 'b.md');
        INSERT INTO pgokf.concept_source VALUES
            (11, 'a', convert_to('# A', 'UTF8')),
            (11, 'b', convert_to('# B', 'UTF8'));
        CREATE FUNCTION pgokf.get_config() RETURNS jsonb
            LANGUAGE sql STABLE
            AS $$ SELECT jsonb_build_object('store_source',
                    (SELECT f.store_source FROM pgokf.fixture f)) $$;
        CREATE FUNCTION pgokf.register_bundle_content(
            p_name text, p_paths text[], p_contents bytea[], p_meta jsonb)
        RETURNS TABLE (bundle_id bigint, added integer, updated integer, removed integer)
        LANGUAGE plpgsql
        AS $register_bundle_content$
        DECLARE
            v_id bigint;
        BEGIN
            SELECT b.id INTO v_id
            FROM pgokf.bundles b
            WHERE b.name = p_name AND b.source_type = 'content'
              AND b.tenant_id
                  = coalesce(nullif(current_setting('pgokf.tenant', true), ''), 'default');
            IF pg_try_advisory_lock(hashtext('pgokf.content_bundle'), hashtext(p_name)) THEN
                PERFORM pg_advisory_unlock(hashtext('pgokf.content_bundle'), hashtext(p_name));
                INSERT INTO pgokf.calls VALUES ('register_bundle_content',
                    p_name || ' UNLOCKED paths=' || coalesce(array_length(p_paths, 1), 0));
            ELSE
                INSERT INTO pgokf.calls VALUES ('register_bundle_content',
                    p_name || ' locked paths=' || coalesce(array_length(p_paths, 1), 0));
            END IF;
            RETURN QUERY SELECT v_id, 0, coalesce(array_length(p_paths, 1), 0), 0;
        END
        $register_bundle_content$;
        CREATE FUNCTION pgokf.refresh_bundle(id bigint)
        RETURNS TABLE (bundle_id bigint, added integer, updated integer, removed integer)
        LANGUAGE plpgsql
        AS $refresh_bundle$
        BEGIN
            INSERT INTO pgokf.calls VALUES ('refresh_bundle', id::text);
            RETURN QUERY SELECT id, 1, 0, 0;
        END
        $refresh_bundle$;";

    /// The scratch server, the fixture loaded into a fresh scratch
    /// database, and the pooled reader/writer over it - or `None` (with a
    /// notice) when no local `PostgreSQL` answers. `PGOKF_WEB_TEST_DB`
    /// overrides the default connection string. The admin client (connected
    /// to the maintenance database) comes back for the final drop; the
    /// fixture client is the scratch-database connection the assertions
    /// read the recorded calls through.
    async fn refresh_scratch() -> Option<(
        tokio_postgres::Client,
        tokio_postgres::Client,
        crate::db::Db,
    )> {
        let base: tokio_postgres::Config = std::env::var("PGOKF_WEB_TEST_DB")
            .unwrap_or_else(|_| {
                let user = std::env::var("USER").unwrap_or_else(|_| "postgres".to_owned());
                format!("host=localhost dbname=postgres user={user}")
            })
            .parse()
            .expect("PGOKF_WEB_TEST_DB parses as a libpq connection string");
        let (admin, connection) = match base.connect(tokio_postgres::NoTls).await {
            Ok(pair) => pair,
            Err(error) => {
                eprintln!(
                    "skipping the content-refresh regression: no scratch PostgreSQL answers ({error})"
                );
                return None;
            }
        };
        tokio::spawn(async move {
            if let Err(error) = connection.await {
                eprintln!("scratch admin connection error: {error}");
            }
        });
        admin
            .batch_execute(&format!("DROP DATABASE IF EXISTS {REFRESH_SCRATCH_DB}"))
            .await
            .expect("drop a stale scratch database");
        admin
            .batch_execute(&format!("CREATE DATABASE {REFRESH_SCRATCH_DB}"))
            .await
            .expect("create the scratch database");
        let mut scratch = base.clone();
        scratch.dbname(REFRESH_SCRATCH_DB);
        let (fixture, connection) = scratch
            .connect(tokio_postgres::NoTls)
            .await
            .expect("connect to the scratch database");
        tokio::spawn(async move {
            if let Err(error) = connection.await {
                eprintln!("scratch fixture connection error: {error}");
            }
        });
        fixture
            .batch_execute(REFRESH_SCRATCH_FIXTURES)
            .await
            .expect("load the refresh fixture");
        let host = match base.get_hosts().first() {
            Some(tokio_postgres::config::Host::Tcp(name)) => name.clone(),
            _ => "localhost".to_owned(),
        };
        let user = base.get_user().unwrap_or("postgres").to_owned();
        let url = format!("host={host} dbname={REFRESH_SCRATCH_DB} user={user}");
        let db = crate::db::Db::connect(&crate::db::DbConfig {
            database_url: &url,
            force_tls: false,
            pool_size: 4,
            tenant: None,
            statement_timeout_ms: 10_000,
        })
        .expect("a pool over the scratch database");
        Some((admin, fixture, db))
    }

    /// What the stand-in writer functions recorded `(fn, detail)`.
    async fn recorded_calls(client: &tokio_postgres::Client) -> Vec<(String, String)> {
        client
            .query("SELECT fn, detail FROM pgokf.calls ORDER BY ctid", &[])
            .await
            .expect("read the recorded calls")
            .iter()
            .map(|row| (row.get(0), row.get(1)))
            .collect()
    }

    #[tokio::test]
    async fn content_bundle_refresh_rebuilds_through_the_locked_resync() {
        let Some((admin, fixture, db)) = refresh_scratch().await else {
            return;
        };
        let mut app = test_app(Some(db.clone()));
        app.db = db.clone();

        // A content bundle's refresh is the document workflow's
        // full-snapshot resync: every stored file re-registered by name,
        // under the bundle's cross-writer lock - never a filesystem
        // re-read.
        let outcome = match refresh_one_bundle(&app, &db, 11).await {
            Ok(outcome) => outcome,
            Err(error) => panic!("the content refresh rebuilds: {}", error.message()),
        };
        assert_eq!(outcome.bundle_id, 11);
        assert_eq!(outcome.updated, 2, "both stored files re-registered");
        assert_eq!(
            recorded_calls(&fixture).await,
            vec![(
                "register_bundle_content".to_owned(),
                "notes locked paths=2".to_owned()
            )]
        );

        // With store_source off the action answers with the workflow's
        // readable refusal (naming the admin step) before any write.
        fixture
            .execute("UPDATE pgokf.fixture SET store_source = false", &[])
            .await
            .expect("switch store_source off");
        fixture
            .execute("TRUNCATE pgokf.calls", &[])
            .await
            .expect("reset the recorded calls");
        let error = refresh_one_bundle(&app, &db, 11)
            .await
            .expect_err("store_source off refuses the rebuild");
        assert_eq!(error.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert!(
            error.message().contains("store_source"),
            "the refusal names the setting: {}",
            error.message()
        );
        assert!(
            error.message().contains("set_config"),
            "the refusal names the admin step: {}",
            error.message()
        );
        assert_eq!(recorded_calls(&fixture).await, Vec::new());

        // A filesystem bundle's refresh is unchanged: the source re-read,
        // no content resync.
        let outcome = match refresh_one_bundle(&app, &db, 12).await {
            Ok(outcome) => outcome,
            Err(error) => panic!(
                "the filesystem refresh re-reads the source: {}",
                error.message()
            ),
        };
        assert_eq!(outcome.bundle_id, 12);
        assert_eq!(outcome.added, 1);
        assert_eq!(
            recorded_calls(&fixture).await,
            vec![("refresh_bundle".to_owned(), "12".to_owned())]
        );

        drop(db);
        drop(fixture);
        admin
            .batch_execute(&format!("DROP DATABASE {REFRESH_SCRATCH_DB} WITH (FORCE)"))
            .await
            .expect("drop the scratch database");
    }

    // ---- Registry ---------------------------------------------------------

    fn registry_repo(id: &str, project: &str, status: &str) -> RegistryRepository {
        RegistryRepository {
            id: id.to_owned(),
            key: "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef".to_owned(),
            project: project.to_owned(),
            branch: "main".to_owned(),
            remote: Some(format!("https://github.com/example/{project}.git")),
            status: status.to_owned(),
            poll_interval_seconds: 300,
            last_indexed_commit: Some("a94a8fe5ccb19ba61c4c0873d391e987982fbbd3".to_owned()),
            last_published_commit: Some("a94a8fe5ccb19ba61c4c0873d391e987982fbbd3".to_owned()),
            last_published_generation: Some(41),
        }
    }

    fn admin_registry_page(rows: Vec<RegistryRow>) -> AdminRegistryPage {
        AdminRegistryPage {
            shell: Shell::bare("Administration · Registry"),
            admin: AdminTab::Registry.shell(AdminOutcome::default()),
            rows,
            registry_known: true,
            registry_note: None,
            writable: true,
            producer_configured: true,
            credentials_known: true,
            credential_note: None,
        }
    }

    #[test]
    fn admin_registry_page_renders_the_table_the_links_and_the_status_pills() {
        // Arrange: one repository per credential shape - set, anonymous,
        // one whose read was refused, and one the producer reports unusable
        // (no last four; the state renders explicitly) - plus a paused one.
        let page = admin_registry_page(vec![
            RegistryRow::new(
                registry_repo("8d2e1c4a-0000-4000-8000-0000000000aa", "atlas", "active"),
                CredentialCell::set(CredentialInfo {
                    label: "deploy key".to_owned(),
                    kind: "github_pat".to_owned(),
                    secret_last4: Some("a1b2".to_owned()),
                    state: "configured".to_owned(),
                    updated_at: "2026-09-13T10:00:00Z".to_owned(),
                }),
            ),
            RegistryRow::new(
                registry_repo("8d2e1c4a-0000-4000-8000-0000000000bb", "beacon", "paused"),
                CredentialCell::anonymous(),
            ),
            RegistryRow::new(
                registry_repo("8d2e1c4a-0000-4000-8000-0000000000cc", "cirrus", "active"),
                CredentialCell::unknown(),
            ),
            RegistryRow::new(
                registry_repo("8d2e1c4a-0000-4000-8000-0000000000dd", "drizzle", "active"),
                CredentialCell::set(CredentialInfo {
                    label: "legacy key".to_owned(),
                    kind: "http_basic".to_owned(),
                    secret_last4: None,
                    state: "unusable".to_owned(),
                    updated_at: "2026-09-01T09:00:00Z".to_owned(),
                }),
            ),
        ]);

        // Act
        let rendered = page.render().expect("the admin registry page renders");

        // Assert: the standing-rule table, one row per repository, with the
        // registry columns wired.
        assert!(rendered.contains("<table class=\"table admin\" id=\"pgokf-admin-registry\">"));
        for column in [
            "<th>Project</th>",
            "<th>Key</th>",
            "<th>Branch</th>",
            "<th>Remote</th>",
            "<th>Status</th>",
            "<th>Credential</th>",
            "<th>Last indexed</th>",
            "<th>Last published</th>",
        ] {
            assert!(rendered.contains(column), "the table heads {column}");
        }
        // Each row's project and remote link to the repository's own page.
        assert!(rendered.contains(
            "<a href=\"/admin/registry/8d2e1c4a-0000-4000-8000-0000000000aa\"><strong>atlas</strong></a>"
        ));
        assert!(rendered.contains(
            "<a href=\"/admin/registry/8d2e1c4a-0000-4000-8000-0000000000aa\" class=\"path\" title=\"https://github.com/example/atlas.git\">https://github.com/example/atlas.git</a>"
        ));
        assert!(rendered.contains("<span class=\"pill ok\">active</span>"));
        assert!(rendered.contains("<span class=\"pill warn\">paused</span>"));
        // The credential column is a status pill and nothing more: label,
        // type, and last four - never a secret - "anonymous" where none is
        // set, and an unusable credential says so.
        assert!(rendered.contains(
            "<span class=\"pill ok\" title=\"updated 2026-09-13 10:00 UTC\">deploy key · github_pat · …a1b2</span>"
        ));
        assert!(rendered.contains("<span class=\"pill muted\">anonymous</span>"));
        assert!(rendered.contains("<span class=\"muted\">&mdash;</span>"));
        assert!(rendered.contains(
            "<span class=\"pill warn\" title=\"updated 2026-09-01 09:00 UTC\">legacy key · http_basic · unusable</span>"
        ));
        // Per-row controls: pause/resume and the poll-interval form. The
        // inline credential forms are GONE - no password input, no
        // set/remove action anywhere on the list page.
        assert!(rendered.contains("name=\"action\" value=\"pause\""));
        assert!(rendered.contains("name=\"action\" value=\"resume\""));
        assert!(rendered.contains("name=\"action\" value=\"poll\""));
        assert!(rendered.contains("name=\"poll_interval\" value=\"300\""));
        assert!(!rendered.contains("value=\"set-credential\""));
        assert!(!rendered.contains("value=\"remove-credential\""));
        assert!(!rendered.contains("type=\"password\" name=\"secret\" required"));
        // The add affordance is the panel-head primary button leading to
        // the dedicated page - the pattern the other tabs use (the
        // Providers tab's "Add a provider"); the long form itself is NOT
        // inline here.
        assert!(rendered.contains(
            "<a class=\"btn primary\" href=\"/admin/registry/new\">Add a repository</a>"
        ));
        assert!(!rendered.contains("<details class=\"adder\""));
        assert!(!rendered.contains("name=\"action\" value=\"register\""));
        assert!(!rendered.contains("name=\"checkout_path\""));
    }

    #[test]
    fn admin_repository_new_page_renders_the_registration_form() {
        // Arrange
        let page = AdminRepositoryNewPage {
            shell: Shell::bare("Administration · Add a repository"),
            admin: AdminTab::Registry.shell(AdminOutcome::default()),
            producer_configured: true,
        };

        // Act
        let rendered = page.render().expect("the add-repository page renders");

        // Assert: the registration form with remote, branch, project,
        // checkout path, and the optional credential triple with a
        // never-prefilled secret, posting the unchanged register action.
        assert!(rendered.contains("<h2>Add a repository</h2>"));
        assert!(rendered.contains("href=\"/admin/registry\">All repositories</a>"));
        assert!(rendered.contains(
            "<form method=\"post\" action=\"/admin/registry\" id=\"pgokf-add-repository\" data-once>"
        ));
        assert!(rendered.contains("name=\"action\" value=\"register\""));
        assert!(rendered.contains("name=\"remote\""));
        assert!(rendered.contains("name=\"branch\" value=\"main\""));
        assert!(rendered.contains("name=\"project\""));
        assert!(rendered.contains("name=\"checkout_path\""));
        assert!(rendered.contains("where the producer stages its worktree"));
        assert!(
            rendered.contains("type=\"password\" name=\"secret\" autocomplete=\"new-password\"")
        );
        assert!(!rendered.contains("name=\"secret\" value="));
        // The form honors the producer's bounds and offers only the
        // credential types it accepts.
        assert!(rendered.contains("maxlength=\"200\""));
        assert!(rendered.contains("<option value=\"github_pat\">GitHub token</option>"));
        assert!(rendered.contains("<option value=\"http_basic\">HTTP basic</option>"));
        assert!(
            !rendered.contains("ssh_key"),
            "the producer refuses ssh_key, so the form never offers it"
        );
        // The copy points at the repository page and names the CLI as the
        // scripted alternative.
        assert!(rendered.contains("register CLI remains the scripted alternative"));
    }

    #[test]
    fn admin_repository_new_page_explains_when_no_producer_is_configured() {
        // Arrange
        let page = AdminRepositoryNewPage {
            shell: Shell::bare("Administration · Add a repository"),
            admin: AdminTab::Registry.shell(AdminOutcome::default()),
            producer_configured: false,
        };

        // Act
        let rendered = page.render().expect("the add-repository page renders");

        // Assert: an explanation instead of the form.
        assert!(rendered.contains("Registration needs the producer admin API"));
        assert!(rendered.contains("OKF_PRODUCER_ADMIN_URL"));
        assert!(!rendered.contains("name=\"action\" value=\"register\""));
    }

    #[test]
    fn admin_registry_page_degrades_to_an_explained_unknown() {
        // Arrange: the registry itself unreadable (no producer schema in
        // this database), and a second page whose credential column the
        // producer did not answer for, on a read-only server.
        let mut no_registry = admin_registry_page(Vec::new());
        no_registry.registry_known = false;
        no_registry.registry_note = Some(
            "This database holds no repository registry (the producer service's \
             ast_graph.repository_registry is not present), so there is nothing to configure \
             here."
                .to_owned(),
        );
        let mut producer_down = admin_registry_page(vec![RegistryRow::new(
            registry_repo("8d2e1c4a-0000-4000-8000-0000000000aa", "atlas", "active"),
            CredentialCell::unknown(),
        )]);
        producer_down.writable = false;
        producer_down.credentials_known = false;
        producer_down.credential_note = Some(
            "The producer admin API did not answer, so the credential column is unknown - \
             reload to try again."
                .to_owned(),
        );

        // Act
        let no_registry = no_registry.render().expect("the empty page renders");
        let producer_down = producer_down.render().expect("the degraded page renders");

        // Assert: each note explains, the unknown cells carry the marker,
        // and no write control is offered on a read-only server.
        assert!(no_registry.contains("This database holds no repository registry"));
        assert!(!no_registry.contains("pgokf-admin-registry\""));
        assert!(producer_down.contains("The producer admin API did not answer"));
        assert!(producer_down.contains("OKF_PG_WRITER_URL"));
        assert!(producer_down.contains("<span class=\"muted\">&mdash;</span>"));
        assert!(!producer_down.contains("name=\"action\" value=\"pause\""));
        assert!(!producer_down.contains("name=\"poll_interval\" value="));
        // No inline credential form ever - degraded or not.
        assert!(!producer_down.contains("value=\"set-credential\""));
        assert!(!producer_down.contains("value=\"remove-credential\""));
        // The add affordance stays: the producer is configured, and a
        // registration retries the read's failure honestly on submit.
        assert!(producer_down.contains("href=\"/admin/registry/new\""));
    }

    /// A signed-in admin, for the handler tests.
    fn admin_session() -> Session {
        Session {
            principal: Some(Principal {
                subject: "operator".to_owned(),
                display: "Operator".to_owned(),
                role: Role::Admin,
            }),
            mode: Mode::Users,
            peer: None,
        }
    }

    /// Header-mode auth that signs every request in as an admin, for the
    /// router-level tests (the identity middleware is part of the chain).
    fn header_auth() -> Authenticator {
        Authenticator::Header(crate::auth::HeaderAuth {
            user_header: axum::http::header::HeaderName::from_static("x-user"),
            name_header: None,
            groups_header: Some(axum::http::header::HeaderName::from_static("x-groups")),
            roles: crate::auth::RoleMapping::parse("admins=admin", Role::Viewer)
                .expect("the role mapping parses"),
            trusted: Vec::new(),
            trust_any_peer: true,
        })
    }

    /// A router-level request signed in as an admin through `header_auth`.
    fn admin_request(method: &str, uri: &str, body: Option<String>) -> axum::http::Request<Body> {
        let mut builder = axum::http::Request::builder()
            .method(method)
            .uri(uri)
            .header("x-user", "operator")
            .header("x-groups", "admins");
        if body.is_some() {
            builder = builder.header("content-type", "application/x-www-form-urlencoded");
        }
        builder
            .body(Body::from(body.unwrap_or_default()))
            .expect("the request builds")
    }

    /// Read a whole response body as text.
    async fn body_text(response: Response) -> String {
        let bytes = axum::body::to_bytes(response.into_body(), 1_000_000)
            .await
            .expect("the body reads");
        String::from_utf8_lossy(&bytes).into_owned()
    }

    #[tokio::test]
    async fn a_credential_secret_is_never_rendered_through_the_stack() {
        // Arrange: a mock producer answering the set (201, body in the
        // contract's document shape) and then the details page's follow-up
        // reads (the credential, then the operator detail), and an App
        // whose producer client points at it, whose registry seam serves
        // the one repository the page shows, and whose visibility probe
        // answers "visible".
        let canary = "canary-secret-7f3a9c-never-rendered";
        let repository_id = "8d2e1c4a-0000-4000-8000-0000000000aa";
        let (base, served) = crate::producer::tests::mock_producer(&[
            (
                "201 Created",
                "{\"label\":\"deploy key\",\"type\":\"github_pat\",\"secret_last4\":\"a1b2\",\"state\":\"configured\",\"updated_at\":\"2026-09-13T10:00:00Z\"}",
            ),
            (
                "200 OK",
                "{\"label\":\"deploy key\",\"type\":\"github_pat\",\"secret_last4\":\"a1b2\",\"state\":\"configured\",\"updated_at\":\"2026-09-13T10:00:00Z\"}",
            ),
            (
                "200 OK",
                "{\"repository_id\":\"8d2e1c4a-0000-4000-8000-0000000000aa\",\"checkout_path\":\"/srv/checkouts/atlas\"}",
            ),
        ])
        .await;
        let mut app = test_app(None);
        app.producer = Some(crate::producer::ProducerAdmin::new(&base, "admin-token").unwrap());
        app.auth = header_auth();
        app.registry_visible = Some(true);
        app.registry_rows = Some(vec![registry_repo(repository_id, "atlas", "active")]);
        let router = router(Arc::new(app));

        // Act: set the credential through the real router - identity
        // middleware, cross-site guard, form parsing, handler - with the
        // canary as the secret, on the repository's own page.
        let response = router
            .clone()
            .oneshot(admin_request(
                "POST",
                &format!("/admin/registry/{repository_id}"),
                Some(format!(
                    "action=set-credential&label=deploy+key&kind=github_pat&secret={canary}"
                )),
            ))
            .await
            .expect("the router answers");

        // Assert: the answer is the redirect back to the repository page
        // with a notice - and the canary is in neither the body nor the
        // Location it points at.
        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        let location = response
            .headers()
            .get(header::LOCATION)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_owned();
        assert!(location.starts_with(&format!("/admin/registry/{repository_id}?notice=")));
        assert!(!location.contains(canary), "the Location carries no secret");
        let body = body_text(response).await;
        assert!(!body.contains(canary), "the POST body carries no secret");

        // Act: follow the redirect the way the browser would, through the
        // same router; the registry row comes from the seam, the credential
        // and the checkout path from the mock producer.
        let response = router
            .oneshot(admin_request("GET", &location, None))
            .await
            .expect("the router answers the follow-up");

        // Assert: the page shows the label, type, and last four the
        // producer reports - and the canary nowhere.
        assert_eq!(response.status(), StatusCode::OK);
        let body = body_text(response).await;
        assert!(body.contains("deploy key · github_pat · …a1b2"));
        assert!(body.contains("/srv/checkouts/atlas"));
        assert!(
            !body.contains(canary),
            "no GET response body ever renders the secret"
        );
        // The full secret's last four characters appear only behind the
        // ellipsis marker, never as a prefix or in full.
        assert!(!body.contains(&canary[..canary.len() - 4]));

        // The secret did cross to the producer, once, in the PUT body -
        // proving the flow exercised the real call.
        let captured = served.await.expect("the mock captured the requests");
        assert_eq!(
            captured.len(),
            3,
            "the set call and the details page's two reads, no more"
        );
        assert!(
            captured[0].body.contains(canary),
            "the PUT carried the secret"
        );
        assert!(captured[0].head.starts_with(
            "PUT /admin/repositories/8d2e1c4a-0000-4000-8000-0000000000aa/credential"
        ));
        assert!(captured[1].head.starts_with(
            "GET /admin/repositories/8d2e1c4a-0000-4000-8000-0000000000aa/credential"
        ));
        assert!(
            captured[2]
                .head
                .starts_with("GET /admin/repositories/8d2e1c4a-0000-4000-8000-0000000000aa ")
        );
    }

    #[tokio::test]
    async fn a_refused_credential_set_renders_no_secret_and_no_echoed_body() {
        // Arrange: a misbehaving producer that answers 400 with the whole
        // request body echoed back, secret included; then the details
        // page's follow-up reads (anonymous credential, then the operator
        // detail).
        let canary = "canary-secret-7f3a9c-never-rendered";
        let repository_id = "8d2e1c4a-0000-4000-8000-0000000000aa";
        let echoed =
            format!("{{\"detail\":\"rejected\",\"you_sent\":{{\"secret\":\"{canary}\"}}}}");
        let (base, served) = crate::producer::tests::mock_producer(&[
            ("400 Bad Request", &echoed),
            ("404 Not Found", "{\"detail\":\"no credential\"}"),
            (
                "200 OK",
                "{\"repository_id\":\"8d2e1c4a-0000-4000-8000-0000000000aa\",\"checkout_path\":\"/srv/checkouts/atlas\"}",
            ),
        ])
        .await;
        let mut app = test_app(None);
        app.producer = Some(crate::producer::ProducerAdmin::new(&base, "admin-token").unwrap());
        app.auth = header_auth();
        app.registry_visible = Some(true);
        app.registry_rows = Some(vec![registry_repo(repository_id, "atlas", "active")]);
        let router = router(Arc::new(app));

        // Act
        let response = router
            .oneshot(admin_request(
                "POST",
                &format!("/admin/registry/{repository_id}"),
                Some(format!(
                    "action=set-credential&label=deploy+key&kind=github_pat&secret={canary}"
                )),
            ))
            .await
            .expect("the router answers");

        // Assert: the refusal renders on the repository page with the
        // status quoted and the echoed body - canary included - nowhere.
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let body = body_text(response).await;
        assert!(body.contains("The producer refused the request (HTTP 400)"));
        assert!(!body.contains(canary), "the error page carries no secret");
        assert!(!body.contains(&canary[..canary.len() - 4]));
        let captured = served.await.expect("the mock captured the requests");
        assert_eq!(captured.len(), 3);
    }

    #[tokio::test]
    async fn credential_actions_refuse_a_repository_the_tenant_cannot_see() {
        // Arrange: a mock producer expecting NOTHING - the refusal must
        // happen before any call crosses to the producer - and a visibility
        // probe answering "not visible" (another tenant's row, an unknown
        // id, and a malformed id all look alike here).
        let (base, served) = crate::producer::tests::mock_producer(&[]).await;
        let mut app = test_app(None);
        app.producer = Some(crate::producer::ProducerAdmin::new(&base, "admin-token").unwrap());
        app.registry_visible = Some(false);
        app.registry_rows = Some(Vec::new());
        let app = Arc::new(app);
        let id = "8d2e1c4a-0000-4000-8000-0000000000aa".to_owned();

        // Act: both credential actions on the invisible id.
        let mut results = Vec::new();
        for action in ["set-credential", "remove-credential"] {
            let form = AdminRepositoryForm {
                action: action.to_owned(),
                label: "deploy key".to_owned(),
                kind: "github_pat".to_owned(),
                secret: "canary-secret-7f3a9c-never-rendered".to_owned(),
            };
            results.push(
                admin_repository(
                    State(app.clone()),
                    admin_session(),
                    Path(id.clone()),
                    Form(form),
                )
                .await
                .unwrap_or_else(|e| panic!("the refusal renders as a page: {}", e.message())),
            );
        }

        // Assert: each refuses with the unknown-repository phrasing (no
        // existence leak), and no request reached the producer at all.
        for response in results {
            assert_eq!(response.status(), StatusCode::BAD_REQUEST);
            let body = body_text(response).await;
            assert!(body.contains("The registry lists no repository with that id"));
            assert!(!body.contains("canary-secret-7f3a9c-never-rendered"));
        }
        let captured = served.await.expect("the mock ran");
        assert!(
            captured.is_empty(),
            "the producer was never called for an invisible id"
        );
    }

    #[tokio::test]
    async fn credential_actions_forward_a_visible_repository_normally() {
        // Arrange: the probe answering "visible" and a mock producer
        // accepting the set and the removal.
        let (base, served) =
            crate::producer::tests::mock_producer(&[("201 Created", ""), ("204 No Content", "")])
                .await;
        let mut app = test_app(None);
        app.producer = Some(crate::producer::ProducerAdmin::new(&base, "admin-token").unwrap());
        app.registry_visible = Some(true);
        let app = Arc::new(app);
        let id = "8d2e1c4a-0000-4000-8000-0000000000aa".to_owned();

        // Act
        let set = admin_repository(
            State(app.clone()),
            admin_session(),
            Path(id.clone()),
            Form(AdminRepositoryForm {
                action: "set-credential".to_owned(),
                label: "deploy key".to_owned(),
                kind: "github_pat".to_owned(),
                secret: "a-secret".to_owned(),
            }),
        )
        .await
        .unwrap_or_else(|e| panic!("the set succeeds: {}", e.message()));
        let remove = admin_repository(
            State(app.clone()),
            admin_session(),
            Path(id.clone()),
            Form(AdminRepositoryForm {
                action: "remove-credential".to_owned(),
                ..AdminRepositoryForm {
                    action: String::new(),
                    label: String::new(),
                    kind: String::new(),
                    secret: String::new(),
                }
            }),
        )
        .await
        .unwrap_or_else(|e| panic!("the removal succeeds: {}", e.message()));

        // Assert: both forward and redirect back to the repository page,
        // and the producer saw both.
        assert_eq!(set.status(), StatusCode::SEE_OTHER);
        assert_eq!(remove.status(), StatusCode::SEE_OTHER);
        let captured = served.await.expect("the mock captured the requests");
        assert_eq!(captured.len(), 2);
        assert!(captured[0].head.starts_with("PUT /admin/repositories/"));
        assert!(captured[1].head.starts_with("DELETE /admin/repositories/"));
    }

    #[tokio::test]
    async fn credential_validation_matches_the_producer_bounds() {
        // Arrange: a mock producer expecting exactly one call (the label at
        // the producer's bound, with the whitespace-padded secret, forwards;
        // every invalid form must stop before the producer).
        let id = "8d2e1c4a-0000-4000-8000-0000000000aa".to_owned();
        let (base, served) = crate::producer::tests::mock_producer(&[("201 Created", "")]).await;
        let mut app = test_app(None);
        app.producer = Some(crate::producer::ProducerAdmin::new(&base, "admin-token").unwrap());
        app.registry_visible = Some(true);
        app.registry_rows = Some(Vec::new());
        let app = Arc::new(app);
        let form = |label: String, kind: &str, secret: &str| AdminRepositoryForm {
            action: "set-credential".to_owned(),
            label,
            kind: kind.to_owned(),
            secret: secret.to_owned(),
        };

        // Act & Assert: a 201-character label is refused...
        let too_long = "l".repeat(201);
        let response = admin_repository(
            State(app.clone()),
            admin_session(),
            Path(id.clone()),
            Form(form(too_long, "github_pat", "a-secret")),
        )
        .await
        .unwrap_or_else(|e| panic!("the refusal renders as a page: {}", e.message()));
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert!(body_text(response).await.contains("at most 200 characters"));
        // ...a type the producer refuses is never offered (ssh_key)...
        let response = admin_repository(
            State(app.clone()),
            admin_session(),
            Path(id.clone()),
            Form(form("deploy key".to_owned(), "ssh_key", "a-secret")),
        )
        .await
        .unwrap_or_else(|e| panic!("the refusal renders as a page: {}", e.message()));
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        // ...and an all-whitespace secret is refused.
        let response = admin_repository(
            State(app.clone()),
            admin_session(),
            Path(id.clone()),
            Form(form("deploy key".to_owned(), "github_pat", "   ")),
        )
        .await
        .unwrap_or_else(|e| panic!("the refusal renders as a page: {}", e.message()));
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);

        // Act: a 200-character label with a whitespace-padded secret - both
        // within the producer's bounds - forwards.
        let response = admin_repository(
            State(app.clone()),
            admin_session(),
            Path(id.clone()),
            Form(form("l".repeat(200), "http_basic", " user:password ")),
        )
        .await
        .unwrap_or_else(|e| panic!("the set succeeds: {}", e.message()));
        assert_eq!(response.status(), StatusCode::SEE_OTHER);

        // Assert: exactly one request crossed, and the secret went
        // byte-for-byte as submitted - never trimmed.
        let captured = served.await.expect("the mock captured the request");
        let [request] = captured.try_into().expect("exactly one request");
        assert!(
            request.body.contains("\"secret\":\" user:password \""),
            "the secret crosses unchanged: {}",
            request.body
        );
        assert!(
            request
                .body
                .contains(&format!("\"label\":\"{}\"", "l".repeat(200)))
        );
    }

    #[tokio::test]
    async fn registry_credential_actions_need_a_configured_producer() {
        // Arrange
        let app = test_app(None);
        let form = AdminRepositoryForm {
            action: "set-credential".to_owned(),
            label: "deploy key".to_owned(),
            kind: "github_pat".to_owned(),
            secret: "canary-secret-7f3a9c-never-rendered".to_owned(),
        };

        // Act
        let result = admin_repository(
            State(Arc::new(app)),
            admin_session(),
            Path("8d2e1c4a-0000-4000-8000-0000000000aa".to_owned()),
            Form(form),
        )
        .await;

        // Assert: an honest unavailable, not a silent success.
        let error = result.expect_err("no producer admin API is configured");
        assert_eq!(error.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert!(error.message().contains("OKF_PRODUCER_ADMIN_URL"));
        assert!(
            !error
                .message()
                .contains("canary-secret-7f3a9c-never-rendered")
        );
    }

    /// A details-page fixture: one repository with a configured credential,
    /// the checkout path the producer's operator API reported.
    fn admin_repository_page(
        repo: RegistryRepository,
        credential: CredentialCell,
        checkout_path: Option<String>,
    ) -> AdminRepositoryPage {
        AdminRepositoryPage {
            shell: Shell::bare("Administration · atlas"),
            admin: AdminTab::Registry.shell(AdminOutcome::default()),
            repo: RepositoryDetailView::new(repo, credential),
            producer_configured: true,
            credentials_known: true,
            credential_note: None,
            checkout_path,
        }
    }

    #[test]
    fn admin_repository_page_renders_the_row_and_the_credential_controls() {
        // Arrange: a credentialed repository.
        let page = admin_repository_page(
            registry_repo("8d2e1c4a-0000-4000-8000-0000000000aa", "atlas", "active"),
            CredentialCell::set(CredentialInfo {
                label: "deploy key".to_owned(),
                kind: "github_pat".to_owned(),
                secret_last4: Some("a1b2".to_owned()),
                state: "configured".to_owned(),
                updated_at: "2026-09-13T10:00:00Z".to_owned(),
            }),
            Some("/srv/checkouts/atlas".to_owned()),
        );

        // Act
        let rendered = page.render().expect("the repository page renders");

        // Assert: the identity block carries the row's fields - the remote,
        // branch, project, key, checkout path, status, poll interval, and
        // the FULL last-indexed commit (the table shows the short form).
        assert!(rendered.contains("https://github.com/example/atlas.git"));
        assert!(rendered.contains("<th>Branch</th><td>main</td>"));
        assert!(rendered.contains("/srv/checkouts/atlas"));
        assert!(rendered.contains("<span class=\"pill ok\">active</span>"));
        assert!(rendered.contains("300s"));
        assert!(rendered.contains("a94a8fe5ccb19ba61c4c0873d391e987982fbbd3"));
        // The credential section: the current credential (label, type, last
        // four, updated - never the secret), the set/replace form with a
        // never-prefilled secret, and the confirmed remove.
        assert!(rendered.contains("deploy key · github_pat · …a1b2"));
        assert!(rendered.contains("updated 2026-09-13 10:00 UTC"));
        assert!(rendered.contains(
            "<form method=\"post\" action=\"/admin/registry/8d2e1c4a-0000-4000-8000-0000000000aa\" data-once>"
        ));
        assert!(rendered.contains("name=\"action\" value=\"set-credential\""));
        assert!(
            rendered.contains(
                "type=\"password\" name=\"secret\" required autocomplete=\"new-password\""
            )
        );
        assert!(!rendered.contains("name=\"secret\" value="));
        assert!(rendered.contains(">Replace credential</button>"));
        assert!(rendered.contains("name=\"action\" value=\"remove-credential\""));
        assert!(rendered.contains("data-confirm=\"Remove the credential for atlas?"));
        assert!(rendered.contains("maxlength=\"200\""));
        assert!(
            !rendered.contains("ssh_key"),
            "the producer refuses ssh_key, so the form never offers it"
        );
        // A way back to the Registry tab.
        assert!(rendered.contains("href=\"/admin/registry\">All repositories</a>"));
    }

    #[test]
    fn admin_repository_page_renders_anonymous_unusable_and_unknown_states() {
        // Arrange: an anonymous repository, one whose credential is
        // unusable, and one the producer did not answer for (no checkout
        // path either).
        let anonymous = admin_repository_page(
            registry_repo("8d2e1c4a-0000-4000-8000-0000000000bb", "beacon", "active"),
            CredentialCell::anonymous(),
            None,
        );
        let unusable = admin_repository_page(
            registry_repo("8d2e1c4a-0000-4000-8000-0000000000dd", "drizzle", "active"),
            CredentialCell::set(CredentialInfo {
                label: "legacy key".to_owned(),
                kind: "http_basic".to_owned(),
                secret_last4: None,
                state: "unusable".to_owned(),
                updated_at: "2026-09-01T09:00:00Z".to_owned(),
            }),
            Some("/srv/checkouts/drizzle".to_owned()),
        );
        let mut unknown = admin_repository_page(
            registry_repo("8d2e1c4a-0000-4000-8000-0000000000cc", "cirrus", "active"),
            CredentialCell::unknown(),
            None,
        );
        unknown.credentials_known = false;
        unknown.credential_note = Some(
            "The producer admin API did not answer, so the credential state and the checkout \
             path are unknown - reload to try again."
                .to_owned(),
        );

        // Act
        let anonymous = anonymous.render().expect("the anonymous page renders");
        let unusable = unusable.render().expect("the unusable page renders");
        let unknown = unknown.render().expect("the degraded page renders");

        // Assert: anonymous says so and offers Set (not Replace, no
        // remove); unusable warns and says to set a new one; the degraded
        // page explains and shows the unknown markers.
        assert!(anonymous.contains("No credential is set; the repository fetches anonymously."));
        assert!(anonymous.contains(">Set credential</button>"));
        assert!(!anonymous.contains("value=\"remove-credential\""));
        assert!(anonymous.contains("unknown right now"));
        assert!(unusable.contains("The stored credential is unusable"));
        assert!(unusable.contains("legacy key · http_basic"));
        assert!(unknown.contains("The producer admin API did not answer"));
        assert!(unknown.contains("The credential state is unknown"));
        // The set form stays on every variant: the producer is configured.
        for rendered in [&anonymous, &unusable, &unknown] {
            assert!(rendered.contains("name=\"action\" value=\"set-credential\""));
        }
    }

    #[tokio::test]
    async fn an_unknown_repository_id_is_a_404() {
        // Arrange: the seam serving one repository; the asked-for id is
        // not it (another tenant's id looks exactly the same here).
        let mut app = test_app(None);
        app.auth = header_auth();
        app.registry_rows = Some(vec![registry_repo(
            "8d2e1c4a-0000-4000-8000-0000000000aa",
            "atlas",
            "active",
        )]);
        let router = router(Arc::new(app));

        // Act
        let response = router
            .oneshot(admin_request(
                "GET",
                "/admin/registry/8d2e1c4a-0000-4000-8000-0000000000ff",
                None,
            ))
            .await
            .expect("the router answers");

        // Assert
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn the_old_inline_credential_actions_are_gone() {
        // Arrange
        let mut app = test_app(None);
        app.auth = header_auth();
        app.registry_rows = Some(Vec::new());
        let router = router(Arc::new(app));

        // Act: the actions the inline table forms used to post.
        for action in ["set-credential", "remove-credential"] {
            let response = router
                .clone()
                .oneshot(admin_request(
                    "POST",
                    "/admin/registry",
                    Some(format!(
                        "action={action}&id=8d2e1c4a-0000-4000-8000-0000000000aa&label=deploy+key&kind=github_pat&secret=canary"
                    )),
                ))
                .await
                .expect("the router answers");

            // Assert: an unknown action now - the credential actions live
            // on the repository page only.
            assert_eq!(
                response.status(),
                StatusCode::BAD_REQUEST,
                "{action} is no longer a list-page action"
            );
            let body = body_text(response).await;
            assert!(body.contains("Unknown action"));
            assert!(!body.contains("canary"));
        }
    }

    /// A register form body, urlencoded; the credential fields are appended
    /// by the caller when the flow under test sets one.
    fn register_body(extra: &str) -> String {
        format!(
            "action=register&remote=https%3A%2F%2Fgithub.com%2Fexample%2Fatlas.git&branch=main&project=atlas&checkout_path=%2Fsrv%2Fcheckouts%2Fatlas{extra}"
        )
    }

    /// The registration receipt the mock producer answers with (the
    /// contract's document shape).
    const REGISTER_RECEIPT: &str = "{\"repository_id\":\"8d2e1c4a-0000-4000-8000-0000000000aa\",\"remote_url\":\"https://github.com/example/atlas\",\"repository_key\":\"example-atlas\",\"default_branch\":\"main\",\"checkout_path\":\"/srv/checkouts/atlas\",\"project_name\":\"atlas\",\"graph_id\":\"11111111-2222-3333-4444-555555555555\",\"poll_interval_seconds\":300,\"status\":\"active\",\"tenant_id\":\"default\",\"last_indexed_commit\":null,\"last_published_commit\":null,\"last_published_generation\":null,\"created_at\":\"2026-09-15T10:00:00Z\",\"updated_at\":\"2026-09-15T10:00:00Z\",\"graph_outcome\":\"created\"}";

    #[tokio::test]
    async fn add_repository_registers_then_sets_the_credential_in_order() {
        // Arrange: a mock producer answering the registration (201) and
        // then the credential set (201), and an App with header auth and
        // the registry seams (the redirect target does not render here).
        let canary = "canary-secret-7f3a9c-never-rendered";
        let (base, served) = crate::producer::tests::mock_producer(&[
            ("201 Created", REGISTER_RECEIPT),
            ("201 Created", ""),
        ])
        .await;
        let mut app = test_app(None);
        app.producer = Some(crate::producer::ProducerAdmin::new(&base, "admin-token").unwrap());
        app.auth = header_auth();
        let router = router(Arc::new(app));

        // Act: register with the credential triple filled.
        let response = router
            .oneshot(admin_request(
                "POST",
                "/admin/registry",
                Some(register_body(&format!(
                    "&label=deploy+key&kind=github_pat&secret={canary}"
                ))),
            ))
            .await
            .expect("the router answers");

        // Assert: the answer lands on the new repository's own page, the
        // notice honestly reporting the graph outcome and the credential.
        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        let location = response
            .headers()
            .get(header::LOCATION)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_owned();
        assert!(
            location.starts_with("/admin/registry/8d2e1c4a-0000-4000-8000-0000000000aa?notice=")
        );
        assert!(!location.contains(canary), "the Location carries no secret");
        // The producer saw, in order: the registration (with the derived
        // key and the checkout path), then the credential PUT against the
        // returned repository id - the first reconcile is authenticated.
        let captured = served.await.expect("the mock captured the requests");
        assert_eq!(captured.len(), 2, "register, then set-credential");
        assert!(captured[0].head.starts_with("POST /admin/repositories "));
        assert!(
            captured[0]
                .body
                .contains("\"repository_key\":\"example-atlas\"")
        );
        assert!(
            captured[0]
                .body
                .contains("\"checkout_path\":\"/srv/checkouts/atlas\"")
        );
        assert!(captured[0].body.contains("\"default_branch\":\"main\""));
        assert!(captured[1].head.starts_with(
            "PUT /admin/repositories/8d2e1c4a-0000-4000-8000-0000000000aa/credential"
        ));
        assert!(
            captured[1].body.contains(canary),
            "the PUT carried the secret"
        );
    }

    #[tokio::test]
    async fn add_repository_without_a_credential_registers_only() {
        // Arrange: a mock producer answering the registration alone; any
        // second request lands on a closed listener and fails the test.
        let (base, served) =
            crate::producer::tests::mock_producer(&[("201 Created", REGISTER_RECEIPT)]).await;
        let mut app = test_app(None);
        app.producer = Some(crate::producer::ProducerAdmin::new(&base, "admin-token").unwrap());
        app.auth = header_auth();
        let router = router(Arc::new(app));

        // Act: register with the credential fields empty.
        let response = router
            .oneshot(admin_request(
                "POST",
                "/admin/registry",
                Some(register_body("")),
            ))
            .await
            .expect("the router answers");

        // Assert
        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        let captured = served.await.expect("the mock captured the request");
        let [request] = captured.try_into().expect("exactly one request");
        assert!(request.head.starts_with("POST /admin/repositories "));
    }

    #[tokio::test]
    async fn add_repository_surfaces_the_409_adoption_guard_readably() {
        // Arrange: the producer refusing with the branch-mismatch adoption
        // guard (its raw JSON detail must NOT leak into the page).
        let (base, served) = crate::producer::tests::mock_producer(&[(
            "409 Conflict",
            "{\"detail\":\"graph for project_name 'atlas' already exists with default_branch 'develop' (graph_id=1111); refusing to adopt\"}",
        )])
        .await;
        let mut app = test_app(None);
        app.producer = Some(crate::producer::ProducerAdmin::new(&base, "admin-token").unwrap());
        app.auth = header_auth();
        app.registry_rows = Some(Vec::new());
        let router = router(Arc::new(app));

        // Act
        let response = router
            .oneshot(admin_request(
                "POST",
                "/admin/registry",
                Some(register_body("")),
            ))
            .await
            .expect("the router answers");

        // Assert: a readable notice on the add-repository page, quoting the status
        // and what it means - never the producer's raw body.
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let body = body_text(response).await;
        assert!(body.contains("The producer refused the registration (HTTP 409)"));
        assert!(body.contains("already registered under another tenant"));
        assert!(body.contains("different branch"));
        assert!(!body.contains("{\"detail\""), "no raw dump: {body}");
        assert!(!body.contains("graph_id=1111"), "no echoed detail: {body}");
        let captured = served.await.expect("the mock captured the request");
        assert_eq!(captured.len(), 1);
    }

    #[tokio::test]
    async fn add_repository_surfaces_the_429_rate_limit_readably() {
        // Arrange
        let (base, served) = crate::producer::tests::mock_producer(&[(
            "429 Too Many Requests",
            "{\"detail\":\"admin mutation rate limit exceeded\"}",
        )])
        .await;
        let mut app = test_app(None);
        app.producer = Some(crate::producer::ProducerAdmin::new(&base, "admin-token").unwrap());
        app.auth = header_auth();
        app.registry_rows = Some(Vec::new());
        let router = router(Arc::new(app));

        // Act
        let response = router
            .oneshot(admin_request(
                "POST",
                "/admin/registry",
                Some(register_body("")),
            ))
            .await
            .expect("the router answers");

        // Assert
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        let body = body_text(response).await;
        assert!(body.contains("rate limit was reached (HTTP 429)"));
        assert!(!body.contains("{\"detail\""), "no raw dump: {body}");
        let captured = served.await.expect("the mock captured the request");
        assert_eq!(captured.len(), 1);
    }

    #[tokio::test]
    async fn add_repository_validates_before_the_producer_is_called() {
        // Arrange: a mock producer expecting NOTHING.
        let (base, served) = crate::producer::tests::mock_producer(&[]).await;
        let mut app = test_app(None);
        app.producer = Some(crate::producer::ProducerAdmin::new(&base, "admin-token").unwrap());
        app.auth = header_auth();
        app.registry_rows = Some(Vec::new());
        let router = router(Arc::new(app));

        // Act & Assert: a non-https remote, a credential-bearing remote, a
        // missing project name, and a half-filled credential are each a
        // readable form error before anything crosses to the producer.
        for (body, expected) in [
            (
                "action=register&remote=git%40github.com%3Aexample%2Fatlas.git&branch=main&project=atlas&checkout_path=%2Fsrv%2Fcheckouts%2Fatlas",
                "must be an explicit https:// URL",
            ),
            (
                "action=register&remote=https%3A%2F%2Fuser%3Atoken%40github.com%2Fexample%2Fatlas.git&branch=main&project=atlas&checkout_path=%2Fsrv%2Fcheckouts%2Fatlas",
                "must not embed credentials",
            ),
            (
                "action=register&remote=https%3A%2F%2Fgithub.com%2Fexample%2Fatlas.git&branch=main&project=&checkout_path=%2Fsrv%2Fcheckouts%2Fatlas",
                "Give the project name",
            ),
            (
                "action=register&remote=https%3A%2F%2Fgithub.com%2Fexample%2Fatlas.git&branch=main&project=atlas&checkout_path=%2Fsrv%2Fcheckouts%2Fatlas&label=deploy+key&kind=github_pat",
                "Give the credential&#39;s secret",
            ),
        ] {
            let response = router
                .clone()
                .oneshot(admin_request(
                    "POST",
                    "/admin/registry",
                    Some(body.to_owned()),
                ))
                .await
                .expect("the router answers");
            assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{expected}");
            assert!(
                body_text(response).await.contains(expected),
                "the refusal says: {expected}"
            );
        }
        let captured = served.await.expect("the mock ran");
        assert!(captured.is_empty(), "the producer was never called");
    }

    #[test]
    fn repository_key_derivation_mirrors_the_producer() {
        // The producer's `default_repository_key` examples and edges: the
        // last two path segments (a trailing `.git` dropped), lowercased,
        // non-alphanumeric runs collapsed to one `-`, capped at 64, the
        // host as the no-path fallback.
        assert_eq!(
            derive_repository_key("https://github.com/LogicOcean/code-graph").as_deref(),
            Some("logicocean-code-graph")
        );
        assert_eq!(
            derive_repository_key("https://github.com/example/atlas.git").as_deref(),
            Some("example-atlas")
        );
        assert_eq!(
            derive_repository_key("https://git.example.com/atlas.git").as_deref(),
            Some("atlas")
        );
        assert_eq!(
            derive_repository_key("https://example.com/My_Repo--Fork/").as_deref(),
            Some("my-repo-fork")
        );
        assert_eq!(
            derive_repository_key("https://example.com").as_deref(),
            Some("example-com")
        );
        let long = derive_repository_key(&format!(
            "https://example.com/{}/{}",
            "o".repeat(40),
            "r".repeat(40)
        ));
        assert_eq!(long.as_ref().map(String::len), Some(64), "capped at 64");
    }

    /// The render half of the credential-document contract (the parse and
    /// status half is `the_producer_credential_contract_deserializes_and_maps_statuses`
    /// in producer.rs): fixtures verbatim in shape from the producer's
    /// `CredentialInfoResponse`
    /// (the producer admin API), served through
    /// the real client, render as the contract intends - the `configured`
    /// one with its last four, the `unusable` one (null `secret_last4`)
    /// labeled, never an unexplained em dash. The producer repo mirrors
    /// this test; a drift in the document shape must fail a test on either
    /// side.
    #[tokio::test]
    async fn the_producer_contract_renders_both_credential_states() {
        // Arrange
        let (base, served) = crate::producer::tests::mock_producer(&[
            (
                "200 OK",
                "{\"label\":\"deploy key\",\"type\":\"github_pat\",\"secret_last4\":\"a1b2\",\"state\":\"configured\",\"updated_at\":\"2026-09-13T10:00:00Z\"}",
            ),
            (
                "200 OK",
                "{\"label\":\"legacy key\",\"type\":\"http_basic\",\"secret_last4\":null,\"state\":\"unusable\",\"updated_at\":\"2026-09-01T09:00:00Z\"}",
            ),
        ])
        .await;
        let producer = crate::producer::ProducerAdmin::new(&base, "admin-token").unwrap();

        // Act
        let configured = producer
            .credential("8d2e1c4a-0000-4000-8000-0000000000aa")
            .await
            .expect("the configured read succeeds")
            .expect("a credential is set");
        let unusable = producer
            .credential("8d2e1c4a-0000-4000-8000-0000000000bb")
            .await
            .expect("the unusable read succeeds")
            .expect("a credential is set");
        let _ = served.await;
        let page = admin_registry_page(vec![
            RegistryRow::new(
                registry_repo("8d2e1c4a-0000-4000-8000-0000000000aa", "atlas", "active"),
                CredentialCell::set(configured),
            ),
            RegistryRow::new(
                registry_repo("8d2e1c4a-0000-4000-8000-0000000000bb", "beacon", "active"),
                CredentialCell::set(unusable),
            ),
        ]);
        let rendered = page.render().expect("the page renders");

        // Assert
        assert!(rendered.contains("deploy key · github_pat · …a1b2</span>"));
        assert!(
            rendered.contains("legacy key · http_basic · unusable</span>"),
            "an unusable credential says so instead of showing a bare gap"
        );
    }

    /// The scratch database the live router regression creates and drops:
    /// the credential actions run through the real router and a real pooled
    /// `Db` per tenant against a stand-in of the producer's registry table.
    const ROUTER_SCRATCH_DB: &str = "pgokf_web_registry_router_test";

    /// One tenant's app for the live router regression: a real pooled `Db`
    /// scoped to the tenant against the scratch registry, the mock
    /// producer, and header auth signing every request in as an admin.
    fn per_tenant_router(url: &str, base: &str, tenant: &str) -> Router {
        let mut app = test_app(None);
        app.db = Db::connect(&crate::db::DbConfig {
            database_url: url,
            force_tls: false,
            pool_size: 1,
            tenant: Some(tenant),
            statement_timeout_ms: 1000,
        })
        .expect("the pooled Db connects to the scratch registry");
        app.tenant = Some(tenant.to_owned());
        app.auth = header_auth();
        app.producer = Some(crate::producer::ProducerAdmin::new(base, "admin-token").unwrap());
        router(Arc::new(app))
    }

    /// The credential actions through the real router and a real database,
    /// for both tenants: a valid set and remove answer with the success
    /// redirect and reach the listening mock producer exactly once each,
    /// while a malformed id, another tenant's id, and an unknown id are
    /// all refused identically before anything is forwarded. This guards
    /// the binding the `App` seams above stub out: the id reaches
    /// `PostgreSQL` as text and is cast in SQL (`$1::text::uuid`), so a
    /// real credential action is not a 500 at the pool. Skips with a notice
    /// when no scratch `PostgreSQL` answers, exactly like the db.rs
    /// execution regressions (`PGOKF_WEB_TEST_DB` overrides the local
    /// default).
    #[tokio::test]
    #[allow(clippy::too_many_lines)]
    async fn credential_actions_run_through_the_real_database_tenant_confined() {
        // Arrange: the scratch database with one registered repository per
        // tenant (tenant-c intentionally has none, so its refusal pages
        // render without a producer read).
        let url = std::env::var("PGOKF_WEB_TEST_DB").unwrap_or_else(|_| {
            let user = std::env::var("USER").unwrap_or_else(|_| "postgres".to_owned());
            format!("host=localhost dbname=postgres user={user}")
        });
        let (admin, connection) = match tokio_postgres::connect(&url, tokio_postgres::NoTls).await {
            Ok(pair) => pair,
            Err(error) => {
                eprintln!("skipping the live router test: no scratch PostgreSQL answers ({error})");
                return;
            }
        };
        tokio::spawn(async move {
            if let Err(error) = connection.await {
                eprintln!("scratch admin connection error: {error}");
            }
        });
        for statement in [
            format!("DROP DATABASE IF EXISTS {ROUTER_SCRATCH_DB} WITH (FORCE)"),
            format!("CREATE DATABASE {ROUTER_SCRATCH_DB}"),
        ] {
            admin
                .batch_execute(&statement)
                .await
                .unwrap_or_else(|error| panic!("{statement}: {error}"));
        }
        let result = async {
            let url = format!("{url} dbname={ROUTER_SCRATCH_DB}");
            let (setup, connection) = tokio_postgres::connect(&url, tokio_postgres::NoTls).await?;
            tokio::spawn(async move {
                if let Err(error) = connection.await {
                    eprintln!("scratch fixture connection error: {error}");
                }
            });
            setup
                .batch_execute(
                    "CREATE SCHEMA ast_graph;
                     CREATE TABLE ast_graph.repository_registry (
                         repository_id uuid PRIMARY KEY,
                         repository_key varchar(64) NOT NULL,
                         project_name varchar(255) NOT NULL,
                         default_branch varchar(255) NOT NULL DEFAULT 'main',
                         remote_url text,
                         status varchar(32) NOT NULL DEFAULT 'active',
                         poll_interval_seconds integer NOT NULL DEFAULT 300,
                         last_indexed_commit varchar(64),
                         last_published_commit varchar(64),
                         last_published_generation bigint,
                         tenant_id text NOT NULL DEFAULT 'default'
                     );
                     INSERT INTO ast_graph.repository_registry
                         (repository_id, repository_key, project_name, tenant_id)
                     VALUES
                         ('00000000-0000-4000-8000-000000000001', 'aaa', 'atlas', 'tenant-a'),
                         ('00000000-0000-4000-8000-000000000002', 'bbb', 'beacon', 'tenant-b');",
                )
                .await?;
            // The mock producer answers, in order: tenant A's set and
            // remove, then tenant B's. Any forwarded request beyond those
            // four lands on a closed listener and fails the test.
            let (base, served) = crate::producer::tests::mock_producer(&[
                ("201 Created", ""),
                ("204 No Content", ""),
                ("201 Created", ""),
                ("204 No Content", ""),
            ])
            .await;

            // Act & Assert: both tenants set and remove their own
            // repository's credential through the real router, on the
            // repository's own page; each action answers with the success
            // redirect, never a 500.
            for (tenant, own) in [
                ("tenant-a", "00000000-0000-4000-8000-000000000001"),
                ("tenant-b", "00000000-0000-4000-8000-000000000002"),
            ] {
                let router = per_tenant_router(&url, &base, tenant);
                let set = router
                    .clone()
                    .oneshot(admin_request(
                        "POST",
                        &format!("/admin/registry/{own}"),
                        Some(
                            "action=set-credential&label=deploy+key&kind=github_pat&secret=canary-secret"
                                .to_owned(),
                        ),
                    ))
                    .await
                    .unwrap();
                assert_eq!(
                    set.status(),
                    StatusCode::SEE_OTHER,
                    "{tenant} set-credential answers the success redirect"
                );
                let remove = router
                    .oneshot(admin_request(
                        "POST",
                        &format!("/admin/registry/{own}"),
                        Some("action=remove-credential".to_owned()),
                    ))
                    .await
                    .unwrap();
                assert_eq!(
                    remove.status(),
                    StatusCode::SEE_OTHER,
                    "{tenant} remove-credential answers the success redirect"
                );
            }

            // Act & Assert: from a tenant with no row of its own, a
            // malformed id, another tenant's id, and an unknown id are all
            // the same refusal on the page - never a 500, never forwarded.
            let router = per_tenant_router(&url, &base, "tenant-c");
            for id in [
                "not-a-uuid",
                "00000000-0000-4000-8000-000000000001",
                "00000000-0000-4000-8000-000000000009",
            ] {
                let response = router
                    .clone()
                    .oneshot(admin_request(
                        "POST",
                        &format!("/admin/registry/{id}"),
                        Some(
                            "action=set-credential&label=deploy+key&kind=github_pat&secret=canary-secret"
                                .to_owned(),
                        ),
                    ))
                    .await
                    .unwrap();
                assert_eq!(
                    response.status(),
                    StatusCode::BAD_REQUEST,
                    "{id} is refused on the page, not a 500"
                );
                let body = body_text(response).await;
                assert!(
                    body.contains("no repository with that id"),
                    "{id} earns the same refusal as an unknown id"
                );
            }

            // Assert: exactly the four valid mutations reached the
            // producer, each under the bearer token, the secret in the two
            // set bodies alone.
            let captured = served.await.expect("the mock producer served its answers");
            assert_eq!(
                captured.len(),
                4,
                "only the four valid credential actions reached the producer"
            );
            let (put_a, delete_a, put_b, delete_b) =
                (&captured[0], &captured[1], &captured[2], &captured[3]);
            assert!(
                put_a
                    .head
                    .starts_with("PUT /admin/repositories/00000000-0000-4000-8000-000000000001/credential"),
                "tenant A's set went to its own repository: {}",
                put_a.head.lines().next().unwrap_or_default()
            );
            assert!(put_a.body.contains("canary-secret"));
            assert!(
                delete_a
                    .head
                    .starts_with("DELETE /admin/repositories/00000000-0000-4000-8000-000000000001/credential"),
                "tenant A's remove went to its own repository"
            );
            assert!(
                put_b
                    .head
                    .starts_with("PUT /admin/repositories/00000000-0000-4000-8000-000000000002/credential"),
                "tenant B's set went to its own repository"
            );
            assert!(
                delete_b
                    .head
                    .starts_with("DELETE /admin/repositories/00000000-0000-4000-8000-000000000002/credential"),
                "tenant B's remove went to its own repository"
            );
            Ok::<_, anyhow::Error>(())
        }
        .await;
        admin
            .batch_execute(&format!(
                "DROP DATABASE IF EXISTS {ROUTER_SCRATCH_DB} WITH (FORCE)"
            ))
            .await
            .expect("drop the scratch database");
        result.expect("the live router regression");
    }
}
