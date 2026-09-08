// SPDX-License-Identifier: AGPL-3.0-only
//! MCP over HTTP, for clients that cannot launch a subprocess.
//!
//! One endpoint, `POST /mcp`, carrying the same JSON-RPC messages the stdio
//! transport carries — [`crate::dispatch`] answers both, so they cannot
//! drift apart. This is the MCP Streamable HTTP transport with the parts a
//! request/response server does not need left out: this server never speaks
//! first, so it opens no event stream and issues no session id, and
//! `GET`/`DELETE` on the endpoint are refused as the specification allows.
//!
//! Unlike stdio, where the client launched this process and already holds
//! the connection string, this endpoint is reachable, so it is never open.
//! Every request but the health probe carries a bearer token, and that
//! check runs in a layer **outside** the concurrency limit and before the
//! body is read, so an anonymous caller can neither hold a slot nor make
//! this server buffer a megabyte for it.
//!
//! Authentication is a static token, not OAuth: a 401 says so rather than
//! pointing at an authorization server.

use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use axum::extract::{ConnectInfo, DefaultBodyLimit, Extension, Request as HttpRequest, State};
use axum::http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response as HttpResponse};
use axum::routing::{get, post};
use axum::{Json, Router, middleware};
use serde_json::json;
use tokio::task::JoinHandle;
use tower::limit::GlobalConcurrencyLimitLayer;
use tower_http::cors::CorsLayer;
use tower_http::timeout::RequestBodyTimeoutLayer;

use crate::catalog::Catalog;
use crate::dispatch::{self, Caller};
use crate::tokens::{Bearer, Tokens};

/// The one path that needs no token, so a container's health probe works.
const HEALTH_PATH: &str = "/healthz";

/// The most bytes one request body may carry.
const MAX_BODY: usize = 1024 * 1024;
/// Requests worked on at once, across every connection; the rest queue.
const MAX_IN_FLIGHT: usize = 32;
/// Wall-clock bound on one request, queue time included.
const REQUEST_TIMEOUT: Duration = Duration::from_mins(1);
/// How long a client has to finish sending a body it has started.
const BODY_TIMEOUT: Duration = Duration::from_secs(15);
/// Bound on a statement in the catalog, under [`REQUEST_TIMEOUT`] so the
/// database gives up before the transport does. Dropping the future does
/// not cancel the query; this does.
const STATEMENT_TIMEOUT: Duration = Duration::from_secs(50);
/// How long a browser may cache a preflight answer.
const CORS_MAX_AGE: Duration = Duration::from_mins(10);
/// How often the health probe really asks the catalog, so an anonymous
/// prober cannot turn it into load.
const HEALTH_INTERVAL: Duration = Duration::from_secs(1);
/// How long the health probe waits for that answer. A probe must report
/// quickly; the request budget is far too long for one.
const HEALTH_TIMEOUT: Duration = Duration::from_secs(3);
/// How often refused requests are summarized into the log.
const REFUSAL_LOG_INTERVAL: Duration = Duration::from_secs(1);

/// The `MCP-Protocol-Version` header a Streamable HTTP client may send. It
/// is not read — every revision this server speaks carries these messages
/// unchanged — but a browser client must be allowed to send it.
const PROTOCOL_HEADER: HeaderName = HeaderName::from_static("mcp-protocol-version");

/// What the HTTP transport needs.
pub struct Server {
    pub catalog: Catalog,
    pub tokens: Tokens,
    /// Browser origins allowed to call this server. Empty (the default)
    /// refuses every request that carries an `Origin` at all, which is what
    /// keeps a page in someone's browser from reaching a server on their
    /// network.
    pub allowed_origins: Vec<String>,
    /// The last catalog check, so the health probe costs one query a second
    /// however often it is called.
    health: RwLock<Option<Check>>,
    /// Requests refused for want of a token, summarized rather than logged
    /// one by one.
    refusals: RwLock<(Instant, u64)>,
}

impl Server {
    /// Assemble the transport's state.
    #[must_use]
    pub fn new(catalog: Catalog, tokens: Tokens, allowed_origins: Vec<String>) -> Self {
        Self {
            catalog,
            tokens,
            allowed_origins,
            health: RwLock::new(None),
            refusals: RwLock::new((Instant::now(), 0)),
        }
    }

    /// Why the catalog is not answering, if it is not. The link is never
    /// re-established, so a health probe that reports this lets a
    /// supervisor restart the process rather than leave it up and failing.
    async fn catalog_failure(&self) -> Option<String> {
        {
            if let Ok(seen) = self.health.read()
                && let Some(check) = seen.as_ref()
                && check.at.elapsed() < HEALTH_INTERVAL
            {
                return check.failure.clone();
            }
        }
        let failure = match tokio::time::timeout(HEALTH_TIMEOUT, self.catalog.ping()).await {
            Ok(Ok(())) => None,
            Ok(Err(error)) => Some(format!("{error:#}")),
            Err(_) => Some(format!(
                "the catalog did not answer within {}s",
                HEALTH_TIMEOUT.as_secs()
            )),
        };
        if let Ok(mut seen) = self.health.write() {
            *seen = Some(Check {
                at: Instant::now(),
                failure: failure.clone(),
            });
        }
        failure
    }

    /// Count one refused request, and say so at most once a second: a flood
    /// of them must not become the denial of service itself.
    fn note_refusal(&self, peer: Option<SocketAddr>) {
        let Ok(mut refusals) = self.refusals.write() else {
            return;
        };
        refusals.1 += 1;
        if refusals.0.elapsed() < REFUSAL_LOG_INTERVAL {
            return;
        }
        let from = peer.map_or_else(|| "an unknown address".to_owned(), |peer| peer.to_string());
        eprintln!(
            "pgokf-mcp: refused {} request(s) with no valid token (most recently from {from})",
            refusals.1
        );
        *refusals = (Instant::now(), 0);
    }
}

/// What the catalog said last time it was asked, and when.
struct Check {
    at: Instant,
    failure: Option<String>,
}

type Shared = Arc<Server>;

/// Serve MCP over HTTP until the process is asked to stop.
///
/// # Errors
///
/// An allowed origin is not a valid header value, the statement timeout
/// cannot be set, the address cannot be bound, or the server fails while
/// running.
pub async fn serve(mut server: Server, bind: SocketAddr) -> Result<()> {
    let cors = cors_layer(&server.allowed_origins)?;
    server
        .catalog
        .set_statement_timeout(STATEMENT_TIMEOUT)
        .await?;
    // Nothing re-establishes this link, so when its driver ends the server
    // stops rather than answering every later call with the same failure.
    let driver = server.catalog.take_driver();
    let tokens = server.tokens.count();
    let shared: Shared = Arc::new(server);
    // Layers wrap, so the *last* one added is the outermost, and a layer
    // applies only to the routes named before it. Read the `/mcp` stack from
    // the bottom up: a request is bounded in time, answered outright if it is
    // a browser preflight, authenticated, then its body is buffered whole
    // (bounded in size and time) *before* one of the few shared work slots is
    // taken - so a client trickling its body cannot hold a slot for the whole
    // request budget and starve the rest, including the health probe. The
    // health route and the fallback are added *after* the concurrency limit
    // and body layers, so they carry neither: a flood on `/mcp` can never stop
    // the probe from answering.
    let router = Router::new()
        .route("/mcp", post(rpc).get(no_stream).delete(no_stream))
        .layer(GlobalConcurrencyLimitLayer::new(MAX_IN_FLIGHT))
        .layer(middleware::from_fn(buffer_body))
        .layer(DefaultBodyLimit::max(MAX_BODY))
        .layer(RequestBodyTimeoutLayer::new(BODY_TIMEOUT))
        .route(HEALTH_PATH, get(health))
        .fallback(not_found)
        .layer(middleware::from_fn_with_state(Arc::clone(&shared), guard))
        .layer(cors)
        .layer(middleware::from_fn(timeout))
        .with_state(Arc::clone(&shared));
    let listener = tokio::net::TcpListener::bind(bind)
        .await
        .with_context(|| format!("binding {bind}"))?;
    eprintln!("pgokf-mcp: serving MCP over HTTP on http://{bind}/mcp ({tokens} token(s))");
    if !is_loopback(bind) {
        eprintln!(
            "pgokf-mcp: {bind} is reachable beyond this host and this server speaks plain HTTP; \
             put a TLS-terminating proxy in front of it"
        );
    }
    let shutdown = pgokf_companion::daemon::shutdown_signal()?;
    let lost = Arc::new(AtomicBool::new(false));
    let noticed = Arc::clone(&lost);
    axum::serve(
        listener,
        router.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(async move {
        tokio::select! {
            asked = shutdown => {
                if let Err(error) = asked {
                    eprintln!("pgokf-mcp: shutdown signal error: {error}");
                }
            }
            () = catalog_lost(driver) => {
                noticed.store(true, Ordering::Relaxed);
            }
        }
    })
    .await
    .context("serving HTTP")?;
    if lost.load(Ordering::Relaxed) {
        bail!("the link to PostgreSQL closed; stopping so a supervisor can start again");
    }
    Ok(())
}

/// Resolve when the catalog's connection driver ends, or never when there
/// is none to wait on.
async fn catalog_lost(driver: Option<JoinHandle<()>>) {
    match driver {
        Some(driver) => {
            let _ = driver.await;
        }
        None => std::future::pending().await,
    }
}

/// The browser rules, from the origins the operator named. A browser needs
/// both: the preflight answered, and the response marked readable. Naming
/// none (the default) allows no browser anything, and [`guard`] refuses the
/// request outright.
fn cors_layer(allowed: &[String]) -> Result<CorsLayer> {
    let mut origins = Vec::with_capacity(allowed.len());
    for origin in allowed {
        origins.push(
            HeaderValue::from_str(origin)
                .with_context(|| format!("{origin:?} is not a valid browser origin"))?,
        );
    }
    Ok(CorsLayer::new()
        .allow_origin(origins)
        .allow_methods([Method::POST])
        .allow_headers([
            header::AUTHORIZATION,
            header::CONTENT_TYPE,
            header::ACCEPT,
            PROTOCOL_HEADER,
        ])
        .max_age(CORS_MAX_AGE))
}

/// Bound every request, so one slow query cannot hold a slot for ever. This
/// is the outermost layer, so the budget covers the wait for a slot too: a
/// queued request is answered rather than left waiting.
/// Read the whole request body into memory before the concurrency slot is
/// taken, bounded in both size ([`MAX_BODY`]) and total time ([`BODY_TIMEOUT`]
/// for the whole body, not per frame). A client that trickles its body then
/// holds only a task and its own connection, never one of the few shared work
/// slots, so it cannot stall other callers or the health probe.
async fn buffer_body(request: HttpRequest, next: Next) -> HttpResponse {
    let (parts, body) = request.into_parts();
    let buffered = tokio::time::timeout(BODY_TIMEOUT, axum::body::to_bytes(body, MAX_BODY)).await;
    let bytes = match buffered {
        Ok(Ok(bytes)) => bytes,
        Ok(Err(_)) => {
            return (
                StatusCode::PAYLOAD_TOO_LARGE,
                Json(json!({ "error": "the request body is too large" })),
            )
                .into_response();
        }
        Err(_) => {
            return (
                StatusCode::REQUEST_TIMEOUT,
                Json(json!({ "error": "the request body was too slow to arrive" })),
            )
                .into_response();
        }
    };
    next.run(HttpRequest::from_parts(
        parts,
        axum::body::Body::from(bytes),
    ))
    .await
}

async fn timeout(request: HttpRequest, next: Next) -> HttpResponse {
    match tokio::time::timeout(REQUEST_TIMEOUT, next.run(request)).await {
        Ok(response) => response,
        Err(_) => (
            StatusCode::GATEWAY_TIMEOUT,
            Json(json!({ "error": "the catalog took too long to answer" })),
        )
            .into_response(),
    }
}

/// Who may go further: the origin rule, then the token.
///
/// This runs before the body is read and before a slot is taken, so an
/// anonymous caller costs this server a header parse and nothing else. The
/// bearer it finds travels on the request for the handler.
async fn guard(State(server): State<Shared>, mut request: HttpRequest, next: Next) -> HttpResponse {
    if let Some(refusal) = origin_refusal(&server.allowed_origins, request.headers()) {
        return refusal;
    }
    if request.uri().path() == HEALTH_PATH {
        return next.run(request).await;
    }
    let peer = request
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|ConnectInfo(peer)| *peer);
    let Some(bearer) = presented_token(request.headers()).and_then(|token| {
        // The tokens file is re-read here, at most once a second, so a
        // revoked token stops working without a restart.
        server.tokens.bearer(&token)
    }) else {
        server.note_refusal(peer);
        return unauthorized();
    };
    request.extensions_mut().insert(bearer);
    next.run(request).await
}

/// Liveness: this process, its tokens file, and its one catalog connection.
/// The body says only whether it is well — what is wrong goes to the log,
/// not to whoever found the port.
async fn health(State(server): State<Shared>) -> HttpResponse {
    let mut wrong = Vec::new();
    if let Some(why) = server.tokens.stale() {
        wrong.push(format!("the tokens file is not being believed: {why}"));
    }
    if let Some(why) = server.catalog_failure().await {
        wrong.push(why);
    }
    if wrong.is_empty() {
        return Json(json!({ "status": "ok" })).into_response();
    }
    eprintln!("pgokf-mcp: unhealthy: {}", wrong.join("; "));
    (
        StatusCode::SERVICE_UNAVAILABLE,
        Json(json!({ "status": "degraded" })),
    )
        .into_response()
}

/// The endpoint opens no event stream, so the specification's other verbs
/// are declined rather than half-implemented.
async fn no_stream() -> HttpResponse {
    (
        StatusCode::METHOD_NOT_ALLOWED,
        [(header::ALLOW, "POST")],
        Json(json!({ "error": "this server sends nothing unsolicited; POST JSON-RPC to /mcp" })),
    )
        .into_response()
}

async fn not_found() -> HttpResponse {
    (
        StatusCode::NOT_FOUND,
        Json(json!({ "error": "not found; the MCP endpoint is POST /mcp" })),
    )
        .into_response()
}

/// One JSON-RPC request from an authenticated caller.
async fn rpc(
    State(server): State<Shared>,
    Extension(bearer): Extension<Bearer>,
    body: String,
) -> HttpResponse {
    let request = match crate::rpc::parse_request(&body) {
        Ok(request) => request,
        Err(response) => return Json(response).into_response(),
    };
    if request.is_notification() {
        // Nothing is ever sent back for a notification; the specification
        // asks for 202 with no body.
        return StatusCode::ACCEPTED.into_response();
    }
    let id = request.reply_id();
    let caller = Caller::Remote(bearer);
    Json(dispatch::handle(&server.catalog, &caller, &request, id).await).into_response()
}

/// The token a request presents, from `Authorization: Bearer`. Only the
/// header is read: a token in a query string would be written to every
/// access log between here and the client.
fn presented_token(headers: &HeaderMap) -> Option<String> {
    let value = headers.get(header::AUTHORIZATION)?.to_str().ok()?;
    let (scheme, token) = value.split_once(' ')?;
    scheme
        .eq_ignore_ascii_case("bearer")
        .then(|| token.trim().to_owned())
        .filter(|token| !token.is_empty())
}

/// A 401 that says a static token is wanted. `error="invalid_token"` marks
/// this as RFC 6750 rather than the start of an OAuth flow, which a bare
/// `Bearer` challenge invites some MCP clients to attempt.
fn unauthorized() -> HttpResponse {
    (
        StatusCode::UNAUTHORIZED,
        [(
            header::WWW_AUTHENTICATE,
            r#"Bearer realm="pgokf-mcp", error="invalid_token""#,
        )],
        Json(json!({
            "error": "this endpoint needs a bearer token from its tokens file, \
                      sent as an Authorization header"
        })),
    )
        .into_response()
}

/// Why a request's `Origin` is refused, if it is. A request without one is
/// not from a browser and passes; a request with one passes only if the
/// operator named that origin, and one whose header cannot even be read is
/// refused rather than waved through.
fn origin_refusal(allowed: &[String], headers: &HeaderMap) -> Option<HttpResponse> {
    let value = headers.get(header::ORIGIN)?;
    if value
        .to_str()
        .is_ok_and(|origin| allowed.iter().any(|named| named == origin))
    {
        return None;
    }
    Some(
        (
            StatusCode::FORBIDDEN,
            Json(json!({ "error": "this origin may not call this server" })),
        )
            .into_response(),
    )
}

/// Whether an address is reachable only from this host.
fn is_loopback(bind: SocketAddr) -> bool {
    match bind.ip() {
        IpAddr::V4(address) => address.is_loopback(),
        IpAddr::V6(address) => address.is_loopback(),
    }
}

/// Refuse an origin list this server could not act on, rather than accept
/// it and quietly allow nothing.
///
/// # Errors
///
/// An entry that is not a valid header value.
pub fn parse_origins(list: &str) -> Result<Vec<String>> {
    let mut origins = Vec::new();
    for origin in list.split(',').map(str::trim).filter(|o| !o.is_empty()) {
        if origin.contains(char::is_whitespace) || !origin.is_ascii() {
            bail!("{origin:?} is not a valid browser origin");
        }
        origins.push(origin.to_owned());
    }
    Ok(origins)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers(pairs: &[(header::HeaderName, &str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (name, value) in pairs {
            map.insert(name.clone(), HeaderValue::from_str(value).expect("header"));
        }
        map
    }

    #[test]
    fn a_token_is_read_only_from_the_authorization_header() {
        // Arrange / Act / Assert
        assert_eq!(
            presented_token(&headers(&[(header::AUTHORIZATION, "Bearer pgokf_abc")])).as_deref(),
            Some("pgokf_abc")
        );
        assert_eq!(
            presented_token(&headers(&[(header::AUTHORIZATION, "bearer  pgokf_abc ")])).as_deref(),
            Some("pgokf_abc"),
            "the scheme is case-insensitive and the value is trimmed"
        );
        assert!(presented_token(&headers(&[(header::AUTHORIZATION, "Basic abc")])).is_none());
        assert!(presented_token(&headers(&[(header::AUTHORIZATION, "Bearer ")])).is_none());
        assert!(presented_token(&HeaderMap::new()).is_none());
    }

    #[test]
    fn a_request_from_a_browser_is_refused_unless_its_origin_was_named() {
        // Arrange
        let named = vec!["https://studio.example".to_owned()];

        // Act / Assert
        assert!(
            origin_refusal(&[], &HeaderMap::new()).is_none(),
            "not a browser"
        );
        assert!(
            origin_refusal(&[], &headers(&[(header::ORIGIN, "http://evil.example")])).is_some()
        );
        assert!(
            origin_refusal(
                &named,
                &headers(&[(header::ORIGIN, "https://studio.example")])
            )
            .is_none()
        );
        assert!(
            origin_refusal(
                &named,
                &headers(&[(header::ORIGIN, "https://evil.example")])
            )
            .is_some()
        );
        assert!(
            origin_refusal(&named, &headers(&[(header::ORIGIN, "null")])).is_some(),
            "an opaque origin is still an origin"
        );
    }

    #[test]
    fn an_origin_header_that_cannot_be_read_is_refused_not_waved_through() {
        // Arrange: bytes a browser would never send, which is exactly why a
        // security check must not treat them as "no origin at all".
        let mut map = HeaderMap::new();
        map.insert(
            header::ORIGIN,
            HeaderValue::from_bytes(b"http://\xff.example").expect("header"),
        );

        // Act / Assert
        assert!(origin_refusal(&["http://ok.example".to_owned()], &map).is_some());
    }

    #[test]
    fn an_origin_list_is_read_or_refused_outright() {
        // Arrange / Act / Assert
        assert_eq!(parse_origins("").expect("empty"), Vec::<String>::new());
        assert_eq!(
            parse_origins(" https://a.example , https://b.example ").expect("two"),
            ["https://a.example", "https://b.example"]
        );
        assert!(parse_origins("https://a.example, bad origin").is_err());
        assert!(parse_origins("https://é.example").is_err());
        assert!(cors_layer(&parse_origins("https://a.example").expect("one")).is_ok());
    }

    #[test]
    fn only_a_loopback_bind_needs_no_warning() {
        // Arrange / Act / Assert
        assert!(is_loopback("127.0.0.1:8081".parse().expect("address")));
        assert!(is_loopback("[::1]:8081".parse().expect("address")));
        assert!(!is_loopback("0.0.0.0:8081".parse().expect("address")));
        assert!(!is_loopback("10.0.0.4:8081".parse().expect("address")));
    }
}
