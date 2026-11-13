// SPDX-License-Identifier: AGPL-3.0-only
//! The HTTP endpoint Forge delivers Confluence events to.
//!
//! Each `POST` to [`EVENTS_PATH`] carries a Forge Invocation Token as its
//! bearer token and, because the manifest asks for it, the app system token
//! in `x-forge-oauth-system`. A delivery is verified, decoded, and handed to
//! an [`EventSink`] within Forge's 5-second budget; the answer tells Forge
//! whether to retry:
//!
//! | Outcome | Status | Forge retries |
//! | --- | --- | --- |
//! | Event accepted by the sink | 202 | no |
//! | Self-generated or unsubscribed event | 204 | no |
//! | Missing or invalid invocation token | 401 | yes, then gives up |
//! | Body larger than Forge ever sends | 413 | yes, then gives up |
//! | Body declared as something other than JSON | 415 | yes, then gives up |
//! | Body that is not a known event | 422 | yes, then gives up |
//! | Keys unreadable or not yet read again, sink failed or slow | 503 | yes |
//!
//! The body's size is judged as it is read, then the token, then the
//! declared media type, then the body itself, so a caller without a valid
//! token learns nothing about what it sent. Every 401 carries a `Bearer`
//! challenge. A body that declares no media type is judged by its content.
//! Neither token is ever written to a log or an answer.

use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use serde_json::json;
use tower_http::timeout::RequestBodyTimeoutLayer;

use super::event::{Decoded, Delivery, EventKind, decode_delivery};
use super::fit::{FitVerifier, Invocation, KeySource};

/// Where the Forge app's endpoint module routes events (`route.path`).
pub const EVENTS_PATH: &str = "/forge/confluence/events";
/// Answers without a token, for a container's health probe.
pub const HEALTH_PATH: &str = "/healthz";
/// The header Forge puts the app system token in.
const SYSTEM_TOKEN_HEADER: &str = "x-forge-oauth-system";
/// The challenge a 401 carries when no bearer token came (RFC 6750).
const CHALLENGE: &str = r#"Bearer realm="pgokf-confluence-events""#;
/// The challenge a 401 carries when the token did not verify (RFC 6750).
const INVALID_TOKEN_CHALLENGE: &str =
    r#"Bearer realm="pgokf-confluence-events", error="invalid_token""#;
/// Forge does not deliver events larger than 200 kB; anything bigger is not
/// from Forge.
const MAX_BODY: usize = 256 * 1024;
/// How long a sender has to finish a body it has started.
const BODY_TIMEOUT: Duration = Duration::from_secs(4);
/// How long the sink may take before the delivery is answered 503 so Forge
/// retries; under Forge's 5-second invocation timeout.
const SINK_TIMEOUT: Duration = Duration::from_secs(4);

/// The app system token that came with a delivery, for calling Confluence as
/// the app. It is never shown by `Debug` and must never be logged.
#[derive(Clone, PartialEq, Eq)]
pub struct SystemToken(String);

impl SystemToken {
    /// The token itself, for an `Authorization: Bearer` header.
    #[must_use]
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for SystemToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SystemToken(..)")
    }
}

/// One verified, decoded delivery.
#[derive(Debug, Clone)]
pub struct Accepted {
    pub invocation: Invocation,
    pub delivery: Delivery,
    pub system_token: Option<SystemToken>,
}

/// Why a sink could not take an event; the delivery is retried.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SinkError(pub String);

/// Shown with control characters escaped, as it is logged as one line.
impl std::fmt::Display for SinkError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&super::printable(&self.0))
    }
}

impl std::error::Error for SinkError {}

/// Where accepted events go. An implementation must be quick - enqueue,
/// don't process - and idempotent per [`super::event::Event::dedupe_key`],
/// because Forge redelivers whatever it did not see acknowledged.
pub trait EventSink: Send + Sync + 'static {
    fn accept(&self, accepted: Accepted) -> impl Future<Output = Result<(), SinkError>> + Send;
}

/// The verifier and sink one endpoint serves with.
pub struct Receiver<K, S> {
    verifier: FitVerifier<K>,
    sink: S,
}

impl<K: KeySource + 'static, S: EventSink> Receiver<K, S> {
    #[must_use]
    pub fn new(verifier: FitVerifier<K>, sink: S) -> Self {
        Self { verifier, sink }
    }
}

/// The routes: [`EVENTS_PATH`] and [`HEALTH_PATH`].
///
/// Layers apply only to the routes added before them, so the health route,
/// added after the body limits, answers even while deliveries are refused.
pub fn router<K: KeySource + 'static, S: EventSink>(receiver: Arc<Receiver<K, S>>) -> Router {
    Router::new()
        .route(EVENTS_PATH, post(receive::<K, S>))
        .layer(DefaultBodyLimit::max(MAX_BODY))
        .layer(RequestBodyTimeoutLayer::new(BODY_TIMEOUT))
        .route(HEALTH_PATH, get(health))
        .fallback(not_found)
        .with_state(receiver)
}

async fn receive<K: KeySource + 'static, S: EventSink>(
    State(receiver): State<Arc<Receiver<K, S>>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let Some(token) = bearer(&headers) else {
        return unauthorized(CHALLENGE, "missing invocation token");
    };
    let invocation = match receiver.verifier.verify(token).await {
        Ok(invocation) => invocation,
        Err(error) if error.is_transient() => {
            eprintln!("pgokf-confluence-events: delivery deferred: {error}");
            return refuse(StatusCode::SERVICE_UNAVAILABLE, "signing keys unavailable");
        }
        Err(error) => {
            eprintln!("pgokf-confluence-events: delivery refused: {error}");
            return unauthorized(INVALID_TOKEN_CHALLENGE, "invalid invocation token");
        }
    };
    if !json_or_undeclared(&headers) {
        return refuse(StatusCode::UNSUPPORTED_MEDIA_TYPE, "deliveries are JSON");
    }
    let delivery = match decode_delivery(&body) {
        Ok(Decoded::Event(delivery)) => *delivery,
        Ok(Decoded::SelfGenerated | Decoded::Unsupported(_)) => {
            return StatusCode::NO_CONTENT.into_response();
        }
        Err(error) => {
            eprintln!("pgokf-confluence-events: delivery refused: {error}");
            return refuse(StatusCode::UNPROCESSABLE_ENTITY, "not a Confluence event");
        }
    };
    let system_token = headers
        .get(SYSTEM_TOKEN_HEADER)
        .and_then(|value| value.to_str().ok())
        .filter(|value| !value.is_empty())
        .map(|value| SystemToken(value.to_owned()));
    let accepted = Accepted {
        invocation,
        delivery,
        system_token,
    };
    match tokio::time::timeout(SINK_TIMEOUT, receiver.sink.accept(accepted)).await {
        Ok(Ok(())) => StatusCode::ACCEPTED.into_response(),
        Ok(Err(error)) => {
            eprintln!("pgokf-confluence-events: delivery deferred: {error}");
            refuse(StatusCode::SERVICE_UNAVAILABLE, "event not stored")
        }
        Err(_) => {
            eprintln!("pgokf-confluence-events: delivery deferred: the sink took too long");
            refuse(StatusCode::SERVICE_UNAVAILABLE, "event not stored")
        }
    }
}

/// The token from an `Authorization: Bearer <token>` header.
fn bearer(headers: &HeaderMap) -> Option<&str> {
    let value = headers.get(header::AUTHORIZATION)?.to_str().ok()?;
    let (scheme, token) = value.split_once(' ')?;
    (scheme.eq_ignore_ascii_case("bearer") && !token.trim().is_empty()).then(|| token.trim())
}

/// Whether the body is declared as JSON - `application/json` or a `+json`
/// type, in any case and with any parameters - or not declared at all, in
/// which case its content decides (RFC 9110, section 8.3).
fn json_or_undeclared(headers: &HeaderMap) -> bool {
    let Some(declared) = headers.get(header::CONTENT_TYPE) else {
        return true;
    };
    let Ok(declared) = declared.to_str() else {
        return false;
    };
    let essence = declared
        .split(';')
        .next()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase();
    essence.strip_prefix("application/").is_some_and(|subtype| {
        subtype == "json"
            || subtype
                .strip_suffix("+json")
                .is_some_and(|prefix| !prefix.is_empty())
    })
}

fn refuse(status: StatusCode, reason: &'static str) -> Response {
    (status, axum::Json(json!({ "error": reason }))).into_response()
}

/// A 401 with the `WWW-Authenticate` challenge RFC 9110 requires of it.
fn unauthorized(challenge: &'static str, reason: &'static str) -> Response {
    let mut response = refuse(StatusCode::UNAUTHORIZED, reason);
    response.headers_mut().insert(
        header::WWW_AUTHENTICATE,
        HeaderValue::from_static(challenge),
    );
    response
}

async fn health() -> Response {
    axum::Json(json!({ "status": "ok" })).into_response()
}

async fn not_found() -> Response {
    refuse(StatusCode::NOT_FOUND, "not found")
}

/// Writes one JSON line per accepted event - what changed, on which page,
/// by whom, and its dedupe key; never a token.
///
/// Each write runs on the blocking pool: a writer that blocks, as standard
/// output does while nothing drains it, then holds a pool thread rather than
/// one of the server's workers, and the sink budget still answers the
/// delivery 503. A line that lands after that answer is a redelivery the
/// consumer drops by its key.
pub struct JsonLinesSink<W> {
    out: Arc<std::sync::Mutex<W>>,
}

impl<W: std::io::Write + Send + 'static> JsonLinesSink<W> {
    #[must_use]
    pub fn new(out: W) -> Self {
        Self {
            out: Arc::new(std::sync::Mutex::new(out)),
        }
    }

    /// The writer, for tests.
    #[cfg(test)]
    fn into_inner(self) -> W {
        Arc::into_inner(self.out)
            .expect("no write in flight")
            .into_inner()
            .unwrap()
    }
}

impl<W: std::io::Write + Send + 'static> EventSink for JsonLinesSink<W> {
    async fn accept(&self, accepted: Accepted) -> Result<(), SinkError> {
        let mut line = describe(&accepted).to_string();
        line.push('\n');
        let out = Arc::clone(&self.out);
        tokio::task::spawn_blocking(move || {
            let mut out = out
                .lock()
                .map_err(|_| SinkError("the event writer is unusable".into()))?;
            out.write_all(line.as_bytes())
                .and_then(|()| out.flush())
                .map_err(|error| SinkError(format!("writing the event: {error}")))
        })
        .await
        .map_err(|_| SinkError("the event writer stopped".into()))?
    }
}

/// The JSON line describing one event.
fn describe(accepted: &Accepted) -> serde_json::Value {
    let event = &accepted.delivery.event;
    let mut line = json!({
        "event": event.kind.name(),
        "dedupe_key": event.dedupe_key(),
        "occurred_at": event.occurred_at,
        "actor": event.actor,
        "page_id": event.page.id,
        "space_id": event.page.space_id,
        "page_version": event.page.version,
        "page_status": event.page.status,
        "live_doc": event.page.live_doc,
        "retry_count": accepted.delivery.retry_count,
        "installation_id": accepted.invocation.installation_id,
    });
    match &event.kind {
        EventKind::Page(_) => {}
        EventKind::Comment {
            comment_id,
            location,
            ..
        } => {
            line["comment_id"] = json!(comment_id);
            line["comment_location"] = json!(location);
        }
        EventKind::Task { task, previous, .. } => {
            line["task"] = json!({
                "id": task.id, "uuid": task.uuid, "complete": task.complete,
                "assignee": task.assignee,
                "was_complete": previous.as_ref().map(|p| p.complete),
            });
        }
    }
    line
}

#[cfg(test)]
mod tests {
    use axum::body::Body;
    use axum::http::Request;
    use serde_json::Value;
    use tower::ServiceExt;

    use super::super::fit::Trust;
    use super::super::fit::testing::{APP_ID, CLOUD_ID, KID, NoKeys, Signer, StaticKeys};
    use super::*;

    /// A sink that records what it was given, or fails on demand.
    #[derive(Default)]
    struct Recording {
        accepted: std::sync::Mutex<Vec<Accepted>>,
        fail: bool,
    }

    impl EventSink for Arc<Recording> {
        async fn accept(&self, accepted: Accepted) -> Result<(), SinkError> {
            if self.fail {
                return Err(SinkError("the queue is full".into()));
            }
            self.accepted.lock().unwrap().push(accepted);
            Ok(())
        }
    }

    fn page_update(self_generated: bool) -> Value {
        json!({"payload": {
            "eventType": "avi:confluence:updated:page",
            "atlassianId": "editor-account",
            "eventCreatedDate": "2026-11-12T06:29:21.907Z",
            "selfGenerated": self_generated,
            "content": {"id": "838205441", "type": "page", "status": "current",
                        "title": "A page", "space": {"id": 42},
                        "version": {"number": 7}}
        }})
    }

    fn app<K: KeySource + 'static>(keys: K, sink: Arc<Recording>) -> Router {
        let verifier = FitVerifier::new(Trust::new(APP_ID, CLOUD_ID).unwrap(), keys);
        router(Arc::new(Receiver::new(verifier, sink)))
    }

    fn delivery(token: Option<&str>, body: Vec<u8>) -> Request<Body> {
        let mut request = Request::post(EVENTS_PATH)
            .header(header::CONTENT_TYPE, "application/json")
            .header(SYSTEM_TOKEN_HEADER, "system-token-value");
        if let Some(token) = token {
            request = request.header(header::AUTHORIZATION, format!("Bearer {token}"));
        }
        request.body(Body::from(body)).unwrap()
    }

    fn json_body(value: &Value) -> Vec<u8> {
        serde_json::to_vec(value).unwrap()
    }

    #[tokio::test]
    async fn receive_hands_a_verified_event_to_the_sink() {
        // Arrange
        let signer = Signer::new(KID);
        let sink = Arc::new(Recording::default());
        let app = app(StaticKeys(signer.jwks()), Arc::clone(&sink));

        // Act
        let response = app
            .oneshot(delivery(
                Some(&signer.token()),
                json_body(&page_update(false)),
            ))
            .await
            .unwrap();

        // Assert
        assert_eq!(response.status(), StatusCode::ACCEPTED);
        let accepted = sink.accepted.lock().unwrap();
        assert_eq!(accepted.len(), 1);
        assert_eq!(accepted[0].delivery.event.page.id, "838205441");
        assert_eq!(
            accepted[0].system_token.as_ref().map(SystemToken::expose),
            Some("system-token-value")
        );
    }

    #[tokio::test]
    async fn receive_refuses_a_delivery_without_a_bearer_token() {
        // Arrange
        let signer = Signer::new(KID);
        let sink = Arc::new(Recording::default());
        let app = app(StaticKeys(signer.jwks()), Arc::clone(&sink));

        // Act
        let response = app
            .oneshot(delivery(None, json_body(&page_update(false))))
            .await
            .unwrap();

        // Assert
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        assert!(sink.accepted.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn receive_refuses_a_token_signed_by_another_key() {
        // Arrange
        let published = Signer::new(KID);
        let impostor = Signer::new(KID);
        let sink = Arc::new(Recording::default());
        let app = app(StaticKeys(published.jwks()), Arc::clone(&sink));

        // Act
        let response = app
            .oneshot(delivery(
                Some(&impostor.token()),
                json_body(&page_update(false)),
            ))
            .await
            .unwrap();

        // Assert
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        assert!(sink.accepted.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn receive_asks_for_a_retry_when_the_keys_are_unreadable() {
        // Arrange
        let signer = Signer::new(KID);
        let sink = Arc::new(Recording::default());
        let app = app(NoKeys, Arc::clone(&sink));

        // Act
        let response = app
            .oneshot(delivery(
                Some(&signer.token()),
                json_body(&page_update(false)),
            ))
            .await
            .unwrap();

        // Assert
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    #[tokio::test]
    async fn receive_acknowledges_a_self_generated_event_without_the_sink() {
        // Arrange
        let signer = Signer::new(KID);
        let sink = Arc::new(Recording::default());
        let app = app(StaticKeys(signer.jwks()), Arc::clone(&sink));

        // Act
        let response = app
            .oneshot(delivery(
                Some(&signer.token()),
                json_body(&page_update(true)),
            ))
            .await
            .unwrap();

        // Assert
        assert_eq!(response.status(), StatusCode::NO_CONTENT);
        assert!(sink.accepted.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn receive_acknowledges_an_unsubscribed_event_without_the_sink() {
        // Arrange
        let signer = Signer::new(KID);
        let sink = Arc::new(Recording::default());
        let app = app(StaticKeys(signer.jwks()), Arc::clone(&sink));
        let mut viewed = page_update(false);
        viewed["payload"]["eventType"] = json!("avi:confluence:viewed:page");

        // Act
        let response = app
            .oneshot(delivery(Some(&signer.token()), json_body(&viewed)))
            .await
            .unwrap();

        // Assert
        assert_eq!(response.status(), StatusCode::NO_CONTENT);
        assert!(sink.accepted.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn receive_refuses_a_body_that_is_not_an_event() {
        // Arrange
        let signer = Signer::new(KID);
        let sink = Arc::new(Recording::default());
        let app = app(StaticKeys(signer.jwks()), Arc::clone(&sink));

        // Act
        let response = app
            .oneshot(delivery(Some(&signer.token()), b"{}".to_vec()))
            .await
            .unwrap();

        // Assert
        assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
    }

    #[tokio::test]
    async fn receive_asks_for_a_retry_when_the_sink_fails() {
        // Arrange
        let signer = Signer::new(KID);
        let sink = Arc::new(Recording {
            fail: true,
            ..Recording::default()
        });
        let app = app(StaticKeys(signer.jwks()), sink);

        // Act
        let response = app
            .oneshot(delivery(
                Some(&signer.token()),
                json_body(&page_update(false)),
            ))
            .await
            .unwrap();

        // Assert
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    #[tokio::test]
    async fn receive_refuses_a_body_larger_than_forge_sends() {
        // Arrange
        let signer = Signer::new(KID);
        let sink = Arc::new(Recording::default());
        let app = app(StaticKeys(signer.jwks()), Arc::clone(&sink));

        // Act
        let response = app
            .oneshot(delivery(Some(&signer.token()), vec![b' '; MAX_BODY + 1]))
            .await
            .unwrap();

        // Assert
        assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
        assert!(sink.accepted.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn health_answers_without_a_token() {
        // Arrange
        let app = app(NoKeys, Arc::new(Recording::default()));

        // Act
        let response = app
            .oneshot(Request::get(HEALTH_PATH).body(Body::empty()).unwrap())
            .await
            .unwrap();

        // Assert
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[test]
    fn system_token_debug_never_shows_the_value() {
        // Arrange
        let token = SystemToken("very-secret".into());

        // Act
        let shown = format!("{token:?}");

        // Assert
        assert!(!shown.contains("very-secret"), "{shown}");
    }

    #[tokio::test]
    async fn json_lines_sink_writes_one_line_per_event_without_tokens() {
        // Arrange
        let sink = JsonLinesSink::new(Vec::new());
        let decoded = decode_delivery(&json_body(&page_update(false))).unwrap();
        let Decoded::Event(delivery) = decoded else {
            panic!("{decoded:?}")
        };
        let accepted = Accepted {
            invocation: Invocation {
                installation_id: "ari:cloud:ecosystem::installation/1".into(),
                api_base_url: "https://api.atlassian.com/ex/confluence/x".parse().unwrap(),
            },
            delivery: *delivery,
            system_token: Some(SystemToken("very-secret".into())),
        };

        // Act
        sink.accept(accepted).await.unwrap();

        // Assert
        let written = String::from_utf8(sink.into_inner()).unwrap();
        assert_eq!(written.lines().count(), 1);
        assert!(!written.contains("very-secret"));
        let line: Value = serde_json::from_str(written.trim_end()).unwrap();
        assert_eq!(line["event"], "page:updated");
        assert_eq!(line["page_id"], "838205441");
        assert_eq!(line["page_version"], 7);
        assert_eq!(line["actor"], "editor-account");
    }
}
