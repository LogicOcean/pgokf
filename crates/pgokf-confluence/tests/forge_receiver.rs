// SPDX-License-Identifier: AGPL-3.0-only
//! The Forge event receiver end to end, through the router the
//! `pgokf-confluence-events` binary serves.
//!
//! Every delivery carries a Forge Invocation Token signed by a throwaway RSA
//! key that the test publishes as Atlassian's, and each case is judged by the
//! status Forge would see and by what reached the sink. Refusals are checked
//! beside an acceptance on the same receiver, so no test can pass by
//! refusing everything.

use std::collections::BTreeSet;
use std::io::Write;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Mutex, mpsc};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use aws_lc_rs::rand::SystemRandom;
use aws_lc_rs::rsa::{KeyPair, KeySize, PublicKeyComponents};
use aws_lc_rs::signature::{KeyPair as _, RSA_PKCS1_SHA256};
use axum::Router;
use axum::body::Body;
use axum::http::{HeaderMap, Method, Request, StatusCode, header};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use jsonwebtoken::jwk::JwkSet;
use pgokf_confluence::forge::event::{Decoded, decode_delivery};
use pgokf_confluence::forge::fit::{FitError, FitVerifier, KeySource, Trust};
use pgokf_confluence::forge::receiver::{
    Accepted, EVENTS_PATH, EventSink, HEALTH_PATH, JsonLinesSink, Receiver, SinkError, router,
};
use serde_json::{Value, json};
use tower::ServiceExt;

const APP_ID: &str = "ari:cloud:ecosystem::app/44444444-4444-4444-8444-444444444444";
const OTHER_APP_ID: &str = "ari:cloud:ecosystem::app/11111111-1111-4111-8111-111111111111";
const CLOUD_ID: &str = "33333333-3333-4333-8333-333333333333";
const OTHER_CLOUD_ID: &str = "22222222-2222-4222-8222-222222222222";
const INSTALLATION_ID: &str =
    "ari:cloud:ecosystem::installation/55555555-5555-4555-8555-555555555555";
const SYSTEM_TOKEN: &str = "forge-app-system-token-7f3c9d1e5b";
const SYSTEM_TOKEN_HEADER: &str = "x-forge-oauth-system";
const PAGE_ID: &str = "838205441";
const UPDATED: &str = "avi:confluence:updated:page";

/// Atlassian's current signing key, as far as these tests are concerned.
static PUBLISHED: LazyLock<Signer> =
    LazyLock::new(|| Signer::new("forge/invocation-token/wf-current"));
/// A different key that claims the published key's ID.
static IMPOSTOR: LazyLock<Signer> =
    LazyLock::new(|| Signer::new("forge/invocation-token/wf-current"));
/// The key Atlassian rotates to.
static ROTATED: LazyLock<Signer> = LazyLock::new(|| Signer::new("forge/invocation-token/wf-next"));
/// Unique `jti` claims, so no two tokens in a run are alike.
static JTI: AtomicU64 = AtomicU64::new(1);

/// A throwaway RSA signing key and the key ID it is published under.
struct Signer {
    key: KeyPair,
    kid: String,
}

impl Signer {
    fn new(kid: &str) -> Self {
        Self {
            key: KeyPair::generate(KeySize::Rsa2048).expect("RSA key generation"),
            kid: kid.to_owned(),
        }
    }

    fn modulus(&self) -> Vec<u8> {
        PublicKeyComponents::<Vec<u8>>::from(self.key.public_key()).n
    }

    /// This key as a JWK, the form Atlassian publishes its keys in.
    fn jwk(&self) -> Value {
        let public = PublicKeyComponents::<Vec<u8>>::from(self.key.public_key());
        json!({
            "kid": self.kid, "kty": "RSA", "alg": "RS256", "use": "sig",
            "n": URL_SAFE_NO_PAD.encode(&public.n),
            "e": URL_SAFE_NO_PAD.encode(&public.e),
        })
    }

    fn header(&self) -> Value {
        json!({"alg": "RS256", "typ": "JWT", "kid": self.kid})
    }

    fn sign(&self, header: &Value, claims: &Value) -> String {
        let signing_input = format!(
            "{}.{}",
            URL_SAFE_NO_PAD.encode(serde_json::to_vec(header).unwrap()),
            URL_SAFE_NO_PAD.encode(serde_json::to_vec(claims).unwrap())
        );
        let mut signature = vec![0; self.key.public_modulus_len()];
        self.key
            .sign(
                &RSA_PKCS1_SHA256,
                &SystemRandom::new(),
                signing_input.as_bytes(),
                &mut signature,
            )
            .expect("RSA signing");
        format!("{signing_input}.{}", URL_SAFE_NO_PAD.encode(signature))
    }

    /// A valid invocation token for this app on this site, issued now.
    fn token(&self) -> String {
        self.sign(&self.header(), &claims())
    }

    fn token_with(&self, claims: &Value) -> String {
        self.sign(&self.header(), claims)
    }
}

fn key_set(signers: &[&Signer]) -> JwkSet {
    let keys: Vec<Value> = signers.iter().map(|signer| signer.jwk()).collect();
    serde_json::from_value(json!({ "keys": keys })).expect("a well-formed key set")
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

/// The claims of an invocation token that comes with an event for this app,
/// installed on this site.
fn claims() -> Value {
    let now = now();
    let api_base_url = format!("https://api.atlassian.com/ex/confluence/{CLOUD_ID}");
    json!({
        "app": {
            "id": APP_ID,
            "installationId": INSTALLATION_ID,
            "apiBaseUrl": api_base_url,
            "version": "3",
            "environment": {"type": "PRODUCTION", "id": "e-1"},
            "module": {"type": "core:endpoint", "key": "pgokf-confluence-events"},
            "installation": {
                "id": INSTALLATION_ID,
                "contexts": [{"name": "confluence", "apiBaseUrl": api_base_url, "cloudId": CLOUD_ID}]
            }
        },
        "aud": APP_ID,
        "iss": "forge/invocation-token",
        "iat": now,
        "nbf": now,
        "exp": now + 25,
        "jti": format!("{:040x}", JTI.fetch_add(1, Ordering::Relaxed)),
    })
}

/// Atlassian's keys as each test publishes them; records the key IDs the
/// verifier asks for.
#[derive(Clone)]
struct Published {
    keys: Arc<Mutex<Result<JwkSet, FitError>>>,
    asked: Arc<Mutex<Vec<String>>>,
}

impl Published {
    fn keys(signers: &[&Signer]) -> Self {
        Self {
            keys: Arc::new(Mutex::new(Ok(key_set(signers)))),
            asked: Arc::default(),
        }
    }

    fn unreadable() -> Self {
        Self {
            keys: Arc::new(Mutex::new(Err(FitError::KeysUnavailable(
                "connection refused".into(),
            )))),
            asked: Arc::default(),
        }
    }

    /// Replace the published keys, as a rotation does.
    fn publish(&self, signers: &[&Signer]) {
        *self.keys.lock().unwrap() = Ok(key_set(signers));
    }

    fn asked(&self) -> Vec<String> {
        self.asked.lock().unwrap().clone()
    }
}

impl KeySource for Published {
    async fn keys_for(&self, kid: &str) -> Result<JwkSet, FitError> {
        self.asked.lock().unwrap().push(kid.to_owned());
        self.keys.lock().unwrap().clone()
    }
}

/// A sink that records what it was handed, or fails when told to.
#[derive(Clone, Default)]
struct Recorder {
    accepted: Arc<Mutex<Vec<Accepted>>>,
    failing: bool,
}

impl Recorder {
    fn accepted(&self) -> Vec<Accepted> {
        self.accepted.lock().unwrap().clone()
    }
}

impl EventSink for Recorder {
    async fn accept(&self, accepted: Accepted) -> Result<(), SinkError> {
        if self.failing {
            return Err(SinkError("the queue is full".into()));
        }
        self.accepted.lock().unwrap().push(accepted);
        Ok(())
    }
}

/// Everything a [`JsonLinesSink`] wrote, shared with the test.
#[derive(Clone, Default)]
struct Captured(Arc<Mutex<Vec<u8>>>);

impl Captured {
    fn text(&self) -> String {
        String::from_utf8(self.0.lock().unwrap().clone()).expect("UTF-8 output")
    }

    fn lines(&self) -> Vec<Value> {
        self.text()
            .lines()
            .map(|line| serde_json::from_str(line).expect("each line is one JSON value"))
            .collect()
    }
}

impl Write for Captured {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn app<K: KeySource + 'static, S: EventSink>(keys: K, sink: S) -> Router {
    let trust = Trust::new(APP_ID, CLOUD_ID).expect("the documented forms");
    router(Arc::new(Receiver::new(FitVerifier::new(trust, keys), sink)))
}

/// A delivery as Forge sends it: a JSON body, the invocation token as its
/// bearer token, and the app system token.
fn delivery(token: &str, body: &Value) -> Request<Body> {
    with_headers(
        &[
            ("content-type", "application/json"),
            ("authorization", &format!("Bearer {token}")),
            (SYSTEM_TOKEN_HEADER, SYSTEM_TOKEN),
        ],
        serde_json::to_vec(body).unwrap(),
    )
}

/// A `POST` to the events path with exactly these headers.
fn with_headers(headers: &[(&str, &str)], body: Vec<u8>) -> Request<Body> {
    let mut request = Request::post(EVENTS_PATH);
    for (name, value) in headers {
        request = request.header(*name, *value);
    }
    request.body(Body::from(body)).unwrap()
}

/// What Forge would read back.
struct Answer {
    status: StatusCode,
    headers: HeaderMap,
    body: String,
}

async fn send(app: &Router, request: Request<Body>) -> Answer {
    let response = app.clone().oneshot(request).await.expect("infallible");
    let status = response.status();
    let headers = response.headers().clone();
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("a readable body");
    Answer {
        status,
        headers,
        body: String::from_utf8_lossy(&body).into_owned(),
    }
}

/// Deliver `body` with `token` to a fresh receiver that trusts `keys`.
async fn deliver(keys: Published, token: &str, body: &Value) -> (StatusCode, Vec<Accepted>) {
    let sink = Recorder::default();
    let answer = send(&app(keys, sink.clone()), delivery(token, body)).await;
    (answer.status, sink.accepted())
}

/// `token` must be refused with 401, and a valid token then accepted by the
/// same receiver.
async fn assert_token_refused(keys: Published, token: &str) {
    let sink = Recorder::default();
    let app = app(keys, sink.clone());
    let refused = send(&app, delivery(token, &update())).await;
    assert_eq!(refused.status, StatusCode::UNAUTHORIZED, "{}", refused.body);
    assert!(
        sink.accepted().is_empty(),
        "a refused delivery reached the sink"
    );
    let control = send(&app, delivery(&PUBLISHED.token(), &update())).await;
    assert_eq!(control.status, StatusCode::ACCEPTED, "{}", control.body);
    assert_eq!(sink.accepted().len(), 1);
}

/// `body`, delivered with a valid token, must be refused with 422 and never
/// reach the sink, while a valid event is then accepted.
async fn assert_body_refused(why: &str, body: Vec<u8>) {
    let sink = Recorder::default();
    let app = app(Published::keys(&[&PUBLISHED]), sink.clone());
    let refused = send(
        &app,
        with_headers(
            &[
                ("content-type", "application/json"),
                ("authorization", &format!("Bearer {}", PUBLISHED.token())),
            ],
            body,
        ),
    )
    .await;
    assert_eq!(
        refused.status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "{why}: {}",
        refused.body
    );
    assert!(sink.accepted().is_empty(), "{why}: reached the sink");
    let control = send(&app, delivery(&PUBLISHED.token(), &update())).await;
    assert_eq!(control.status, StatusCode::ACCEPTED, "{why}: control");
}

fn page_event(event_type: &str, version: u64) -> Value {
    json!({"payload": {
        "eventType": event_type,
        "atlassianId": "557058:editor",
        "eventCreatedDate": "2026-11-12T06:29:21.907Z",
        "suppressNotifications": false,
        "selfGenerated": false,
        "updateTrigger": "edit_page",
        "content": {
            "id": PAGE_ID, "type": "page", "status": "current", "title": "A page",
            "space": {"id": 827_392_002, "key": "SP"},
            "version": {"number": version}
        }
    }})
}

fn update() -> Value {
    page_event(UPDATED, 7)
}

fn comment_event(event_type: &str, comment_id: &str) -> Value {
    json!({"payload": {
        "eventType": event_type,
        "atlassianId": "557058:commenter",
        "eventCreatedDate": "2026-11-12T08:00:00Z",
        "content": {
            "id": comment_id, "type": "comment", "status": "current", "title": "Re: A page",
            "space": {"id": 827_392_002},
            "version": {"number": 1},
            "container": {
                "id": PAGE_ID, "type": "page", "status": "current", "title": "A page",
                "space": {"id": 827_392_002}, "version": {"number": 4}
            },
            "extensions": {"location": "footer"}
        }
    }})
}

fn task_event(event_type: &str, status: &str) -> Value {
    json!({"payload": {
        "eventType": event_type,
        "atlassianId": "557058:approver",
        "eventCreatedDate": "2026-11-12T07:00:00Z",
        "task": {"id": 12, "uuid": "66666666-6666-4666-8666-666666666666",
                 "status": status, "assignee": "557058:approver"},
        "oldTask": {"id": 12, "uuid": "66666666-6666-4666-8666-666666666666",
                    "status": "incomplete", "assignee": "557058:approver"},
        "content": {"id": PAGE_ID, "type": "page", "status": "current", "title": "A page",
                    "space": {"id": 827_392_002}}
    }})
}

// ---------------------------------------------------------------------------
// The invocation token's signature
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_token_signed_by_the_published_key_is_accepted() {
    // Arrange
    let token = PUBLISHED.token();

    // Act
    let (status, accepted) = deliver(Published::keys(&[&PUBLISHED]), &token, &update()).await;

    // Assert
    assert_eq!(status, StatusCode::ACCEPTED);
    assert_eq!(accepted.len(), 1);
    assert_eq!(accepted[0].invocation.installation_id, INSTALLATION_ID);
    assert_eq!(accepted[0].delivery.event.page.id, PAGE_ID);
}

#[tokio::test]
async fn a_token_signed_by_another_key_under_the_published_key_id_is_refused() {
    // Arrange
    let token = IMPOSTOR.token();

    // Act / Assert
    assert_token_refused(Published::keys(&[&PUBLISHED]), &token).await;
}

#[tokio::test]
async fn a_token_whose_claims_were_changed_after_signing_is_refused() {
    // Arrange: the claims of a signed token swapped for others, the
    // signature kept.
    let signed = PUBLISHED.token();
    let mut parts: Vec<&str> = signed.split('.').collect();
    let mut changed = claims();
    changed["app"]["installationId"] =
        json!("ari:cloud:ecosystem::installation/ffffffff-ffff-4fff-8fff-ffffffffffff");
    let forged_claims = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&changed).unwrap());
    parts[1] = &forged_claims;

    // Act / Assert
    assert_token_refused(Published::keys(&[&PUBLISHED]), &parts.join(".")).await;
}

#[tokio::test]
async fn a_token_whose_header_was_changed_after_signing_is_refused() {
    // Arrange: signed under the current key, then relabelled with the ID of
    // another published key.
    let signed = PUBLISHED.token();
    let mut parts: Vec<&str> = signed.split('.').collect();
    let relabelled = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&ROTATED.header()).unwrap());
    parts[0] = &relabelled;

    // Act / Assert
    assert_token_refused(Published::keys(&[&PUBLISHED, &ROTATED]), &parts.join(".")).await;
}

#[tokio::test]
async fn a_token_whose_signature_was_changed_is_refused() {
    // Arrange: one bit of the signature flipped.
    let signed = PUBLISHED.token();
    let (signing_input, signature) = signed.rsplit_once('.').unwrap();
    let mut bytes = URL_SAFE_NO_PAD.decode(signature).unwrap();
    bytes[100] ^= 0x01;
    let token = format!("{signing_input}.{}", URL_SAFE_NO_PAD.encode(bytes));

    // Act / Assert
    assert_token_refused(Published::keys(&[&PUBLISHED]), &token).await;
}

#[tokio::test]
async fn an_unsigned_token_is_refused() {
    // Arrange
    let token = format!(
        "{}.{}.",
        URL_SAFE_NO_PAD.encode(br#"{"alg":"none","typ":"JWT"}"#),
        URL_SAFE_NO_PAD.encode(serde_json::to_vec(&claims()).unwrap())
    );

    // Act / Assert
    assert_token_refused(Published::keys(&[&PUBLISHED]), &token).await;
}

#[tokio::test]
async fn a_token_signed_with_the_public_key_as_an_hmac_secret_is_refused() {
    // Arrange: the classic algorithm confusion - an HS256 token whose secret
    // is the published RSA key itself.
    let mut header = jsonwebtoken::Header::new(jsonwebtoken::Algorithm::HS256);
    header.kid = Some(PUBLISHED.kid.clone());
    let token = jsonwebtoken::encode(
        &header,
        &claims(),
        &jsonwebtoken::EncodingKey::from_secret(&PUBLISHED.modulus()),
    )
    .unwrap();

    // Act / Assert
    assert_token_refused(Published::keys(&[&PUBLISHED]), &token).await;
}

#[tokio::test]
async fn an_expired_token_is_refused_beyond_the_clock_leeway() {
    // Arrange
    let mut expired = claims();
    expired["iat"] = json!(now() - 600);
    expired["nbf"] = json!(now() - 600);
    expired["exp"] = json!(now() - 575);
    let mut just_expired = claims();
    just_expired["exp"] = json!(now() - 2);

    // Act
    let (within_leeway, _) = deliver(
        Published::keys(&[&PUBLISHED]),
        &PUBLISHED.token_with(&just_expired),
        &update(),
    )
    .await;

    // Assert
    assert_eq!(within_leeway, StatusCode::ACCEPTED);
    assert_token_refused(
        Published::keys(&[&PUBLISHED]),
        &PUBLISHED.token_with(&expired),
    )
    .await;
}

#[tokio::test]
async fn a_token_that_is_not_yet_valid_is_refused() {
    // Arrange
    let mut early = claims();
    early["nbf"] = json!(now() + 600);
    early["exp"] = json!(now() + 625);

    // Act / Assert
    assert_token_refused(
        Published::keys(&[&PUBLISHED]),
        &PUBLISHED.token_with(&early),
    )
    .await;
}

/// `object` without its member `key`.
fn without(object: &mut Value, key: &str) {
    object.as_object_mut().expect("an object").remove(key);
}

/// One way to spoil something valid, and what it is called.
type Case = (&'static str, fn(&mut Value));

/// Each change to valid claims must get its token refused.
async fn assert_claims_refused(cases: &[Case]) {
    for (why, change) in cases {
        // Arrange
        let mut claims = claims();
        change(&mut claims);

        // Act
        let (status, accepted) = deliver(
            Published::keys(&[&PUBLISHED]),
            &PUBLISHED.token_with(&claims),
            &update(),
        )
        .await;

        // Assert
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{why}");
        assert!(accepted.is_empty(), "{why}");
    }
}

#[tokio::test]
async fn a_token_from_another_issuer_or_without_an_expiry_is_refused() {
    assert_claims_refused(&[
        ("another issuer", |c| c["iss"] = json!("someone-else")),
        ("no issuer", |c| without(c, "iss")),
        ("no expiry", |c| without(c, "exp")),
        ("a textual expiry", |c| c["exp"] = json!("tomorrow")),
    ])
    .await;
}

#[tokio::test]
async fn a_delivery_without_a_bearer_token_is_refused_with_a_bearer_challenge() {
    // Arrange
    let app = app(Published::keys(&[&PUBLISHED]), Recorder::default());
    let body = serde_json::to_vec(&update()).unwrap();
    let requests = [
        with_headers(&[("content-type", "application/json")], body.clone()),
        with_headers(&[("authorization", "Basic dXNlcjpwYXNz")], body.clone()),
        with_headers(&[("authorization", "Bearer   ")], body.clone()),
        Request::post(format!("{EVENTS_PATH}?token={}", PUBLISHED.token()))
            .body(Body::from(body))
            .unwrap(),
    ];

    for request in requests {
        // Act
        let answer = send(&app, request).await;

        // Assert: RFC 9110 requires a 401 to carry a challenge.
        assert_eq!(answer.status, StatusCode::UNAUTHORIZED);
        let challenge = answer
            .headers
            .get(header::WWW_AUTHENTICATE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default();
        assert!(challenge.starts_with("Bearer "), "{challenge:?}");
    }
}

#[tokio::test]
async fn an_invalid_token_is_refused_with_an_invalid_token_challenge() {
    // Arrange
    let app = app(Published::keys(&[&PUBLISHED]), Recorder::default());
    let token = IMPOSTOR.token();

    // Act
    let answer = send(&app, delivery(&token, &update())).await;

    // Assert
    assert_eq!(answer.status, StatusCode::UNAUTHORIZED);
    let challenge = answer
        .headers
        .get(header::WWW_AUTHENTICATE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default();
    assert!(challenge.starts_with("Bearer "), "{challenge:?}");
    assert!(
        challenge.contains(r#"error="invalid_token""#),
        "{challenge:?}"
    );
    assert!(
        !answer.body.contains(&token),
        "the refusal echoes the token"
    );
}

#[tokio::test]
async fn the_bearer_scheme_is_matched_without_regard_to_case() {
    // Arrange
    let sink = Recorder::default();
    let app = app(Published::keys(&[&PUBLISHED]), sink.clone());
    let request = with_headers(
        &[
            ("content-type", "application/json"),
            ("authorization", &format!("bEaReR {}", PUBLISHED.token())),
        ],
        serde_json::to_vec(&update()).unwrap(),
    );

    // Act
    let answer = send(&app, request).await;

    // Assert
    assert_eq!(answer.status, StatusCode::ACCEPTED, "{}", answer.body);
    assert_eq!(sink.accepted().len(), 1);
}

// ---------------------------------------------------------------------------
// Which published key verifies a token, and key rotation
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_key_id_selects_the_verifying_key_among_those_published() {
    // Arrange
    let keys = Published::keys(&[&PUBLISHED, &ROTATED]);
    let sink = Recorder::default();
    let app = app(keys.clone(), sink.clone());

    // Act
    let by_current = send(&app, delivery(&PUBLISHED.token(), &update())).await;
    let by_next = send(&app, delivery(&ROTATED.token(), &update())).await;

    // Assert
    assert_eq!(by_current.status, StatusCode::ACCEPTED);
    assert_eq!(by_next.status, StatusCode::ACCEPTED);
    assert_eq!(sink.accepted().len(), 2);
    assert_eq!(keys.asked(), [PUBLISHED.kid.clone(), ROTATED.kid.clone()]);
}

#[tokio::test]
async fn a_token_naming_one_published_key_but_signed_by_another_is_refused() {
    // Arrange: signed by the next key, labelled with the current one.
    let token = ROTATED.sign(&PUBLISHED.header(), &claims());

    // Act / Assert
    assert_token_refused(Published::keys(&[&PUBLISHED, &ROTATED]), &token).await;
}

#[tokio::test]
async fn a_token_naming_an_unpublished_key_is_refused() {
    // Arrange
    let keys = Published::keys(&[&PUBLISHED]);
    let token = ROTATED.token();

    // Act
    assert_token_refused(keys.clone(), &token).await;

    // Assert: the verifier looked for exactly the key the token named.
    assert_eq!(keys.asked()[0], ROTATED.kid);
}

#[tokio::test]
async fn a_token_naming_no_key_is_refused_without_reading_the_keys() {
    // Arrange
    let keys = Published::keys(&[&PUBLISHED]);
    let token = PUBLISHED.sign(&json!({"alg": "RS256", "typ": "JWT"}), &claims());

    // Act
    let (status, accepted) = deliver(keys.clone(), &token, &update()).await;

    // Assert
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert!(accepted.is_empty());
    assert!(keys.asked().is_empty(), "{:?}", keys.asked());
}

#[tokio::test]
async fn a_rotated_key_is_accepted_once_published_and_a_retired_key_is_refused() {
    // Arrange
    let keys = Published::keys(&[&PUBLISHED]);
    let sink = Recorder::default();
    let app = app(keys.clone(), sink.clone());

    // Act
    let before_rotation = send(&app, delivery(&ROTATED.token(), &update())).await;
    keys.publish(&[&PUBLISHED, &ROTATED]);
    let after_rotation = send(&app, delivery(&ROTATED.token(), &update())).await;
    keys.publish(&[&ROTATED]);
    let retired = send(&app, delivery(&PUBLISHED.token(), &update())).await;

    // Assert
    assert_eq!(before_rotation.status, StatusCode::UNAUTHORIZED);
    assert_eq!(after_rotation.status, StatusCode::ACCEPTED);
    assert_eq!(retired.status, StatusCode::UNAUTHORIZED);
    assert_eq!(sink.accepted().len(), 1);
}

#[tokio::test]
async fn unreadable_keys_defer_the_delivery_for_a_retry() {
    // Arrange
    let token = PUBLISHED.token();

    // Act
    let (status, accepted) = deliver(Published::unreadable(), &token, &update()).await;

    // Assert
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert!(accepted.is_empty());
}

// ---------------------------------------------------------------------------
// The configured Forge app
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_token_issued_for_another_app_is_refused() {
    assert_claims_refused(&[
        ("another audience", |c| c["aud"] = json!(OTHER_APP_ID)),
        ("no audience", |c| without(c, "aud")),
        ("another app claim", |c| {
            c["app"]["id"] = json!(OTHER_APP_ID);
        }),
        ("both naming another app", |c| {
            c["aud"] = json!(OTHER_APP_ID);
            c["app"]["id"] = json!(OTHER_APP_ID);
        }),
        ("no app claim", |c| without(c, "app")),
        ("an app ID in another case", |c| {
            c["app"]["id"] = json!(APP_ID.to_uppercase());
        }),
    ])
    .await;
}

#[tokio::test]
async fn a_token_listing_this_app_among_its_audiences_is_accepted() {
    // Arrange
    let mut claims = claims();
    claims["aud"] = json!([OTHER_APP_ID, APP_ID]);

    // Act
    let (status, accepted) = deliver(
        Published::keys(&[&PUBLISHED]),
        &PUBLISHED.token_with(&claims),
        &update(),
    )
    .await;

    // Assert
    assert_eq!(status, StatusCode::ACCEPTED);
    assert_eq!(accepted.len(), 1);
}

#[test]
fn the_configured_identity_must_take_its_documented_forms() {
    // Arrange / Act / Assert
    assert!(Trust::new(APP_ID, CLOUD_ID).is_ok());
    assert!(Trust::new(APP_ID, &CLOUD_ID.to_uppercase()).is_ok());
    for (app_id, cloud_id) in [
        ("44444444-4444-4444-8444-444444444444", CLOUD_ID),
        ("ari:cloud:ecosystem::app/not-a-uuid", CLOUD_ID),
        (
            "ari:cloud:ecosystem::installation/44444444-4444-4444-8444-444444444444",
            CLOUD_ID,
        ),
        (APP_ID, "not-a-uuid"),
        (APP_ID, ""),
    ] {
        assert!(Trust::new(app_id, cloud_id).is_err(), "{app_id} {cloud_id}");
    }
}

// ---------------------------------------------------------------------------
// The configured Confluence site
// ---------------------------------------------------------------------------

fn with_sites(installation: Option<&[&str]>, context: Option<&str>) -> Value {
    let mut claims = claims();
    match installation {
        Some(sites) => {
            let contexts: Vec<Value> = sites
                .iter()
                .map(|site| json!({"name": "confluence", "cloudId": site}))
                .collect();
            claims["app"]["installation"]["contexts"] = json!(contexts);
        }
        None => without(&mut claims["app"], "installation"),
    }
    if let Some(site) = context {
        claims["context"] = json!({"cloudId": site, "moduleKey": "pgokf-confluence-events"});
    }
    claims
}

#[tokio::test]
async fn a_token_naming_only_this_site_is_accepted_in_either_claim_and_any_case() {
    let upper = CLOUD_ID.to_uppercase();
    for (why, claims) in [
        ("installation", with_sites(Some(&[CLOUD_ID]), None)),
        ("context", with_sites(None, Some(CLOUD_ID))),
        ("both", with_sites(Some(&[CLOUD_ID]), Some(CLOUD_ID))),
        ("upper case", with_sites(Some(&[&upper]), Some(&upper))),
    ] {
        // Act
        let (status, accepted) = deliver(
            Published::keys(&[&PUBLISHED]),
            &PUBLISHED.token_with(&claims),
            &update(),
        )
        .await;

        // Assert
        assert_eq!(status, StatusCode::ACCEPTED, "{why}");
        assert_eq!(accepted.len(), 1, "{why}");
    }
}

#[tokio::test]
async fn a_token_from_an_installation_on_another_site_is_refused() {
    for (why, claims) in [
        ("installation", with_sites(Some(&[OTHER_CLOUD_ID]), None)),
        ("context", with_sites(None, Some(OTHER_CLOUD_ID))),
        ("no site", with_sites(None, None)),
        ("no installation context", with_sites(Some(&[]), None)),
    ] {
        // Act
        let (status, accepted) = deliver(
            Published::keys(&[&PUBLISHED]),
            &PUBLISHED.token_with(&claims),
            &update(),
        )
        .await;

        // Assert
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{why}");
        assert!(accepted.is_empty(), "{why}");
    }
}

#[tokio::test]
async fn a_token_naming_another_site_anywhere_is_refused_even_beside_this_one() {
    for (why, claims) in [
        (
            "installed elsewhere, invoked here",
            with_sites(Some(&[OTHER_CLOUD_ID]), Some(CLOUD_ID)),
        ),
        (
            "installed here, invoked elsewhere",
            with_sites(Some(&[CLOUD_ID]), Some(OTHER_CLOUD_ID)),
        ),
        (
            "installed on both",
            with_sites(Some(&[CLOUD_ID, OTHER_CLOUD_ID]), None),
        ),
    ] {
        // Act
        let (status, accepted) = deliver(
            Published::keys(&[&PUBLISHED]),
            &PUBLISHED.token_with(&claims),
            &update(),
        )
        .await;

        // Assert
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{why}");
        assert!(accepted.is_empty(), "{why}");
    }
}

// ---------------------------------------------------------------------------
// Events the app caused itself
// ---------------------------------------------------------------------------

#[tokio::test]
async fn an_event_the_app_caused_itself_is_acknowledged_without_reaching_the_sink() {
    for mut event in [
        update(),
        comment_event("avi:confluence:created:comment", "838205455"),
        task_event("avi:confluence:updated:task", "complete"),
    ] {
        // Arrange
        event["payload"]["selfGenerated"] = json!(true);
        let out = Captured::default();
        let app = app(
            Published::keys(&[&PUBLISHED]),
            JsonLinesSink::new(out.clone()),
        );

        // Act
        let answer = send(&app, delivery(&PUBLISHED.token(), &event)).await;

        // Assert
        assert_eq!(answer.status, StatusCode::NO_CONTENT, "{event}");
        assert_eq!(out.text(), "", "{event}");
    }
}

#[tokio::test]
async fn an_event_a_person_caused_reaches_the_sink_whether_or_not_it_says_so() {
    // Arrange
    let mut unmarked = update();
    without(&mut unmarked["payload"], "selfGenerated");

    for event in [update(), unmarked] {
        // Act
        let (status, accepted) =
            deliver(Published::keys(&[&PUBLISHED]), &PUBLISHED.token(), &event).await;

        // Assert
        assert_eq!(status, StatusCode::ACCEPTED, "{event}");
        assert_eq!(accepted.len(), 1, "{event}");
    }
}

#[tokio::test]
async fn an_event_the_app_caused_itself_still_needs_a_valid_token() {
    // Arrange
    let mut event = update();
    event["payload"]["selfGenerated"] = json!(true);

    // Act
    let (status, _) = deliver(Published::keys(&[&PUBLISHED]), &IMPOSTOR.token(), &event).await;

    // Assert
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

// ---------------------------------------------------------------------------
// The key that is identical for redeliveries
// ---------------------------------------------------------------------------

/// The dedupe keys the receiver writes for `events`, delivered in order.
async fn keys_of(events: &[Value]) -> Vec<String> {
    let out = Captured::default();
    let app = app(
        Published::keys(&[&PUBLISHED]),
        JsonLinesSink::new(out.clone()),
    );
    for event in events {
        let answer = send(&app, delivery(&PUBLISHED.token(), event)).await;
        assert_eq!(answer.status, StatusCode::ACCEPTED, "{event}");
    }
    out.lines()
        .iter()
        .map(|line| line["dedupe_key"].as_str().unwrap().to_owned())
        .collect()
}

#[tokio::test]
async fn a_redelivered_event_is_written_again_under_the_same_key() {
    // Arrange: the second attempt carries Forge's retry context and, like
    // every attempt, a fresh invocation token.
    let first = update();
    let mut redelivery = first.clone();
    redelivery["payload"]["retryContext"] =
        json!({"retryCount": 1, "retryReason": "TIMEOUT_ERROR", "retryData": null});
    let out = Captured::default();
    let app = app(
        Published::keys(&[&PUBLISHED]),
        JsonLinesSink::new(out.clone()),
    );

    // Act
    let statuses = [
        send(&app, delivery(&PUBLISHED.token(), &first))
            .await
            .status,
        send(&app, delivery(&PUBLISHED.token(), &redelivery))
            .await
            .status,
    ];

    // Assert
    assert_eq!(statuses, [StatusCode::ACCEPTED; 2]);
    let lines = out.lines();
    assert_eq!(lines.len(), 2);
    assert_eq!(lines[0]["dedupe_key"], lines[1]["dedupe_key"]);
    assert_eq!(lines[0]["retry_count"], 0);
    assert_eq!(lines[1]["retry_count"], 1);
}

#[tokio::test]
async fn the_key_ignores_how_the_body_is_serialized() {
    // Arrange: the same event with its members in another order and spaced
    // out, as a different serializer would send it.
    let compact = serde_json::to_string(&update()).unwrap();
    let reordered = format!(
        r#"{{ "payload" : {{ "content" : {{ "version" : {{ "number" : 7 }}, "space" : {{ "key" : "SP", "id" : 827392002 }},
            "title" : "A page", "status" : "current", "type" : "page", "id" : "{PAGE_ID}" }},
            "updateTrigger" : "edit_page", "selfGenerated" : false, "suppressNotifications" : false,
            "eventCreatedDate" : "2026-11-12T06:29:21.907Z", "atlassianId" : "557058:editor",
            "eventType" : "{UPDATED}" }} }}"#
    );
    let out = Captured::default();
    let app = app(
        Published::keys(&[&PUBLISHED]),
        JsonLinesSink::new(out.clone()),
    );

    // Act
    for body in [compact, reordered] {
        let request = with_headers(
            &[
                ("content-type", "application/json"),
                ("authorization", &format!("Bearer {}", PUBLISHED.token())),
            ],
            body.into_bytes(),
        );
        assert_eq!(send(&app, request).await.status, StatusCode::ACCEPTED);
    }

    // Assert
    let lines = out.lines();
    assert_eq!(lines[0]["dedupe_key"], lines[1]["dedupe_key"]);
}

#[tokio::test]
async fn different_events_get_different_keys() {
    // Arrange: each differs from the first in exactly one thing that makes
    // it another event.
    let mut later = update();
    later["payload"]["eventCreatedDate"] = json!("2026-11-12T06:31:00.000Z");
    let events = [
        update(),
        page_event(UPDATED, 8),
        page_event("avi:confluence:moved:page", 7),
        page_event("avi:confluence:permissions_updated:page", 7),
        later,
        comment_event("avi:confluence:created:comment", "838205455"),
        comment_event("avi:confluence:created:comment", "838205456"),
        comment_event("avi:confluence:deleted:comment", "838205455"),
        task_event("avi:confluence:updated:task", "complete"),
        task_event("avi:confluence:updated:task", "incomplete"),
        task_event("avi:confluence:created:task", "incomplete"),
    ];

    // Act
    let keys = keys_of(&events).await;

    // Assert
    assert_eq!(keys.len(), events.len());
    let distinct: BTreeSet<&String> = keys.iter().collect();
    assert_eq!(distinct.len(), keys.len(), "{keys:#?}");
}

#[tokio::test]
async fn the_key_is_a_blake3_digest_in_lower_case_hex() {
    // Act
    let keys = keys_of(&[update()]).await;

    // Assert
    assert_eq!(keys[0].len(), 64, "{}", keys[0]);
    assert!(
        keys[0]
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)),
        "{}",
        keys[0]
    );
}

// ---------------------------------------------------------------------------
// Malformed deliveries
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_body_that_is_not_the_documented_envelope_is_refused() {
    let payload = update()["payload"].clone();
    for (why, body) in [
        ("not JSON", b"not json".to_vec()),
        ("empty", Vec::new()),
        ("JSON null", b"null".to_vec()),
        (
            "a JSON string",
            br#""avi:confluence:updated:page""#.to_vec(),
        ),
        ("a JSON number", b"7".to_vec()),
        ("no payload", serde_json::to_vec(&payload).unwrap()),
        ("a null payload", br#"{"payload":null}"#.to_vec()),
        (
            "a string payload",
            br#"{"payload":"avi:confluence:updated:page"}"#.to_vec(),
        ),
        (
            "an array payload",
            serde_json::to_vec(&json!({"payload": [payload]})).unwrap(),
        ),
        (
            "an array envelope",
            serde_json::to_vec(&json!([payload])).unwrap(),
        ),
        (
            "an array of envelopes",
            serde_json::to_vec(&json!([update()])).unwrap(),
        ),
    ] {
        assert_body_refused(why, body).await;
    }
}

/// `change` applied to a valid page update.
fn changed(change: fn(&mut Value)) -> Vec<u8> {
    let mut event = update();
    change(&mut event);
    serde_json::to_vec(&event).unwrap()
}

#[tokio::test]
async fn a_subscribed_event_missing_what_it_needs_is_refused() {
    let cases: [Case; 13] = [
        ("no event type", |e| without(&mut e["payload"], "eventType")),
        ("a numeric event type", |e| {
            e["payload"]["eventType"] = json!(7);
        }),
        ("no content", |e| without(&mut e["payload"], "content")),
        ("no event time", |e| {
            without(&mut e["payload"], "eventCreatedDate");
        }),
        ("an empty event time", |e| {
            e["payload"]["eventCreatedDate"] = json!("");
        }),
        ("a blank event time", |e| {
            e["payload"]["eventCreatedDate"] = json!(" \t");
        }),
        ("a page without a space", |e| {
            without(&mut e["payload"]["content"], "space");
        }),
        ("a null space ID", |e| {
            e["payload"]["content"]["space"]["id"] = Value::Null;
        }),
        ("an object space ID", |e| {
            e["payload"]["content"]["space"]["id"] = json!({});
        }),
        ("a page without a status", |e| {
            without(&mut e["payload"]["content"], "status");
        }),
        ("a page without an ID", |e| {
            without(&mut e["payload"]["content"], "id");
        }),
        ("a textual version", |e| {
            e["payload"]["content"]["version"]["number"] = json!("7");
        }),
        ("a negative retry count", |e| {
            e["payload"]["retryContext"] = json!({"retryCount": -1});
        }),
    ];
    for (why, change) in cases {
        assert_body_refused(why, changed(change)).await;
    }
}

#[tokio::test]
async fn a_comment_or_task_event_missing_what_it_needs_is_refused() {
    let mut no_container = comment_event("avi:confluence:created:comment", "838205455");
    without(&mut no_container["payload"]["content"], "container");
    let mut no_task = task_event("avi:confluence:updated:task", "complete");
    without(&mut no_task["payload"], "task");
    let mut textual_task_id = task_event("avi:confluence:updated:task", "complete");
    textual_task_id["payload"]["task"]["id"] = json!("12");
    for (why, event) in [
        ("a comment without its page", no_container),
        ("a task event without its task", no_task),
        ("a textual task ID", textual_task_id),
    ] {
        assert_body_refused(why, serde_json::to_vec(&event).unwrap()).await;
    }
}

#[tokio::test]
async fn an_identifier_that_is_not_a_canonical_confluence_id_is_refused() {
    let page_ids = [
        "",
        " ",
        "../838205441",
        "0838205441",
        "83820544a",
        "-838205441",
    ];
    for id in page_ids {
        let mut event = update();
        event["payload"]["content"]["id"] = json!(id);
        assert_body_refused(
            &format!("page ID {id:?}"),
            serde_json::to_vec(&event).unwrap(),
        )
        .await;
    }
    let long = "1".repeat(33);
    let mut event = update();
    event["payload"]["content"]["id"] = json!(long);
    assert_body_refused("a 33-digit page ID", serde_json::to_vec(&event).unwrap()).await;
    for space_id in [json!(-5), json!(0), json!(1.5), json!("SP"), json!("")] {
        let mut event = update();
        event["payload"]["content"]["space"]["id"] = space_id.clone();
        assert_body_refused(
            &format!("space ID {space_id}"),
            serde_json::to_vec(&event).unwrap(),
        )
        .await;
    }
    for (why, field) in [("comment ID", "id"), ("comment page ID", "container")] {
        let mut event = comment_event("avi:confluence:created:comment", "838205455");
        if field == "id" {
            event["payload"]["content"]["id"] = json!("");
        } else {
            event["payload"]["content"]["container"]["id"] = json!("x");
        }
        assert_body_refused(why, serde_json::to_vec(&event).unwrap()).await;
    }
}

#[tokio::test]
async fn canonical_identifiers_are_accepted_as_strings_or_numbers() {
    for space_id in [json!(827_392_002), json!("827392002")] {
        // Arrange
        let mut event = update();
        event["payload"]["content"]["space"]["id"] = space_id.clone();

        // Act
        let (status, accepted) =
            deliver(Published::keys(&[&PUBLISHED]), &PUBLISHED.token(), &event).await;

        // Assert
        assert_eq!(status, StatusCode::ACCEPTED, "{space_id}");
        assert_eq!(accepted[0].delivery.event.page.space_id, "827392002");
    }
}

#[tokio::test]
async fn fields_atlassian_adds_later_are_ignored() {
    // Arrange
    let mut event = update();
    event["payload"]["somethingNew"] = json!({"nested": [1, 2, 3]});
    event["payload"]["content"]["somethingElse"] = json!("value");
    event["envelopeExtra"] = json!(true);

    // Act
    let (status, accepted) =
        deliver(Published::keys(&[&PUBLISHED]), &PUBLISHED.token(), &event).await;

    // Assert
    assert_eq!(status, StatusCode::ACCEPTED);
    assert_eq!(accepted.len(), 1);
}

#[tokio::test]
async fn a_well_formed_event_that_is_not_subscribed_is_acknowledged_without_the_sink() {
    let mut on_blog_post = comment_event("avi:confluence:created:comment", "838205455");
    on_blog_post["payload"]["content"]["container"]["type"] = json!("blogpost");
    let mut task_on_blog_post = task_event("avi:confluence:created:task", "incomplete");
    task_on_blog_post["payload"]["content"]["type"] = json!("blogpost");
    for event in [
        page_event("avi:confluence:viewed:page", 7),
        page_event("avi:jira:updated:issue", 7),
        page_event("AVI:CONFLUENCE:UPDATED:PAGE", 7),
        on_blog_post,
        task_on_blog_post,
    ] {
        // Act
        let (status, accepted) =
            deliver(Published::keys(&[&PUBLISHED]), &PUBLISHED.token(), &event).await;

        // Assert
        assert_eq!(status, StatusCode::NO_CONTENT, "{event}");
        assert!(accepted.is_empty(), "{event}");
    }
}

// ---------------------------------------------------------------------------
// HTTP methods, paths, and media types
// ---------------------------------------------------------------------------

#[tokio::test]
async fn only_post_is_accepted_on_the_events_path() {
    // Arrange
    let sink = Recorder::default();
    let app = app(Published::keys(&[&PUBLISHED]), sink.clone());
    let body = serde_json::to_vec(&update()).unwrap();

    for method in [Method::GET, Method::PUT, Method::DELETE, Method::PATCH] {
        let request = Request::builder()
            .method(method.clone())
            .uri(EVENTS_PATH)
            .header(header::CONTENT_TYPE, "application/json")
            .header(
                header::AUTHORIZATION,
                format!("Bearer {}", PUBLISHED.token()),
            )
            .body(Body::from(body.clone()))
            .unwrap();

        // Act
        let answer = send(&app, request).await;

        // Assert
        assert_eq!(answer.status, StatusCode::METHOD_NOT_ALLOWED, "{method}");
        assert_eq!(
            answer
                .headers
                .get(header::ALLOW)
                .map(|v| v.to_str().unwrap()),
            Some("POST"),
            "{method}"
        );
    }
    assert!(sink.accepted().is_empty());
    let control = send(&app, delivery(&PUBLISHED.token(), &update())).await;
    assert_eq!(control.status, StatusCode::ACCEPTED);
}

#[tokio::test]
async fn the_health_probe_answers_a_get_without_a_token_and_nothing_else() {
    // Arrange: keys that cannot be read must not matter to the probe.
    let app = app(Published::unreadable(), Recorder::default());

    // Act
    let get = send(&app, Request::get(HEALTH_PATH).body(Body::empty()).unwrap()).await;
    let post = send(
        &app,
        Request::post(HEALTH_PATH).body(Body::empty()).unwrap(),
    )
    .await;

    // Assert
    assert_eq!(get.status, StatusCode::OK);
    assert_eq!(
        serde_json::from_str::<Value>(&get.body).unwrap(),
        json!({"status": "ok"})
    );
    assert_eq!(post.status, StatusCode::METHOD_NOT_ALLOWED);
}

#[tokio::test]
async fn another_path_is_not_found() {
    // Arrange
    let app = app(Published::keys(&[&PUBLISHED]), Recorder::default());

    for path in [
        "/",
        "/forge/confluence/events/",
        "/forge/confluence",
        "/healthz/x",
    ] {
        // Act
        let answer = send(&app, Request::post(path).body(Body::empty()).unwrap()).await;

        // Assert
        assert_eq!(answer.status, StatusCode::NOT_FOUND, "{path}");
    }
}

/// A valid delivery with `content_type` as its declared media type, or none.
fn declared_as(content_type: Option<&str>, body: &Value) -> Request<Body> {
    let token = format!("Bearer {}", PUBLISHED.token());
    let mut headers = vec![("authorization", token.as_str())];
    if let Some(content_type) = content_type {
        headers.push(("content-type", content_type));
    }
    with_headers(&headers, serde_json::to_vec(body).unwrap())
}

#[tokio::test]
async fn a_body_declared_as_something_other_than_json_is_refused() {
    // Arrange
    let sink = Recorder::default();
    let app = app(Published::keys(&[&PUBLISHED]), sink.clone());

    for content_type in [
        "text/plain",
        "text/plain; charset=utf-8",
        "application/x-www-form-urlencoded",
        "multipart/form-data; boundary=x",
        "application/xml",
        "application/jsonp",
        "application/json-seq",
        "text/json",
        "application/",
    ] {
        // Act
        let answer = send(&app, declared_as(Some(content_type), &update())).await;

        // Assert
        assert_eq!(
            answer.status,
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "{content_type}: {}",
            answer.body
        );
    }
    assert!(sink.accepted().is_empty());
}

#[tokio::test]
async fn a_json_body_is_accepted_with_parameters_in_any_case_or_undeclared() {
    for content_type in [
        Some("application/json"),
        Some("application/json; charset=utf-8"),
        Some("Application/JSON"),
        Some("application/json ; charset=UTF-8"),
        Some("application/vnd.atlassian.event+json"),
        None,
    ] {
        // Arrange
        let sink = Recorder::default();
        let app = app(Published::keys(&[&PUBLISHED]), sink.clone());

        // Act
        let answer = send(&app, declared_as(content_type, &update())).await;

        // Assert
        assert_eq!(answer.status, StatusCode::ACCEPTED, "{content_type:?}");
        assert_eq!(sink.accepted().len(), 1, "{content_type:?}");
    }
}

#[tokio::test]
async fn an_undeclared_body_is_still_judged_by_its_content() {
    // Arrange
    let app = app(Published::keys(&[&PUBLISHED]), Recorder::default());
    let request = with_headers(
        &[("authorization", &format!("Bearer {}", PUBLISHED.token()))],
        b"page=838205441".to_vec(),
    );

    // Act
    let answer = send(&app, request).await;

    // Assert
    assert_eq!(answer.status, StatusCode::UNPROCESSABLE_ENTITY);
}

#[tokio::test]
async fn the_token_is_judged_before_the_media_type() {
    // Arrange
    let app = app(Published::keys(&[&PUBLISHED]), Recorder::default());
    let request = with_headers(
        &[
            ("content-type", "text/plain"),
            ("authorization", &format!("Bearer {}", IMPOSTOR.token())),
        ],
        serde_json::to_vec(&update()).unwrap(),
    );

    // Act
    let answer = send(&app, request).await;

    // Assert
    assert_eq!(answer.status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn a_body_larger_than_forge_ever_sends_is_refused() {
    // Arrange
    let sink = Recorder::default();
    let app = app(Published::keys(&[&PUBLISHED]), sink.clone());
    let mut event = update();
    event["payload"]["padding"] = json!("x".repeat(300 * 1024));

    // Act
    let answer = send(&app, delivery(&PUBLISHED.token(), &event)).await;

    // Assert
    assert_eq!(answer.status, StatusCode::PAYLOAD_TOO_LARGE);
    assert!(sink.accepted().is_empty());
}

// ---------------------------------------------------------------------------
// The JSON line written for each accepted event
// ---------------------------------------------------------------------------

/// The one line the receiver writes for `event`.
async fn line_for(event: &Value) -> Value {
    let out = Captured::default();
    let app = app(
        Published::keys(&[&PUBLISHED]),
        JsonLinesSink::new(out.clone()),
    );
    let answer = send(&app, delivery(&PUBLISHED.token(), event)).await;
    assert_eq!(answer.status, StatusCode::ACCEPTED, "{}", answer.body);
    let mut lines = out.lines();
    assert_eq!(lines.len(), 1, "{}", out.text());
    lines.remove(0)
}

fn field_names(line: &Value) -> BTreeSet<&str> {
    line.as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect()
}

const PAGE_FIELDS: [&str; 11] = [
    "actor",
    "dedupe_key",
    "event",
    "installation_id",
    "live_doc",
    "occurred_at",
    "page_id",
    "page_status",
    "page_version",
    "retry_count",
    "space_id",
];

#[tokio::test]
async fn a_page_event_is_written_as_one_line_of_exactly_its_fields() {
    // Arrange
    let event = update();
    let expected_key = match decode_delivery(&serde_json::to_vec(&event).unwrap()).unwrap() {
        Decoded::Event(delivery) => delivery.event.dedupe_key(),
        other => panic!("{other:?}"),
    };

    // Act
    let line = line_for(&event).await;

    // Assert
    assert_eq!(field_names(&line), BTreeSet::from(PAGE_FIELDS));
    assert_eq!(
        line,
        json!({
            "event": "page:updated",
            "dedupe_key": expected_key,
            "occurred_at": "2026-11-12T06:29:21.907Z",
            "actor": "557058:editor",
            "page_id": PAGE_ID,
            "space_id": "827392002",
            "page_version": 7,
            "page_status": "current",
            "live_doc": false,
            "retry_count": 0,
            "installation_id": INSTALLATION_ID,
        })
    );
}

#[tokio::test]
async fn a_comment_event_line_names_the_comment_and_its_page() {
    // Act
    let line = line_for(&comment_event(
        "avi:confluence:created:comment",
        "838205455",
    ))
    .await;

    // Assert
    let mut expected: BTreeSet<&str> = BTreeSet::from(PAGE_FIELDS);
    expected.extend(["comment_id", "comment_location"]);
    assert_eq!(field_names(&line), expected);
    assert_eq!(line["event"], "comment:created");
    assert_eq!(line["comment_id"], "838205455");
    assert_eq!(line["comment_location"], "footer");
    assert_eq!(line["page_id"], PAGE_ID);
    assert_eq!(line["page_version"], 4);
}

#[tokio::test]
async fn a_task_event_line_carries_the_task_and_its_previous_state() {
    // Act
    let line = line_for(&task_event("avi:confluence:updated:task", "complete")).await;

    // Assert
    let mut expected: BTreeSet<&str> = BTreeSet::from(PAGE_FIELDS);
    expected.insert("task");
    assert_eq!(field_names(&line), expected);
    assert_eq!(line["event"], "task:updated");
    assert_eq!(
        line["task"],
        json!({"id": 12, "uuid": "66666666-6666-4666-8666-666666666666", "complete": true,
               "assignee": "557058:approver", "was_complete": false})
    );
    assert_eq!(line["page_version"], Value::Null);
}

#[tokio::test]
async fn neither_token_is_ever_written() {
    // Arrange
    let out = Captured::default();
    let app = app(
        Published::keys(&[&PUBLISHED]),
        JsonLinesSink::new(out.clone()),
    );
    let mut tokens = Vec::new();

    for event in [
        update(),
        comment_event("avi:confluence:updated:comment", "838205455"),
        task_event("avi:confluence:removed:task", "incomplete"),
    ] {
        // Act
        let token = PUBLISHED.token();
        let answer = send(&app, delivery(&token, &event)).await;
        assert_eq!(answer.status, StatusCode::ACCEPTED);
        tokens.push(token);
    }

    // Assert: not the system token, and no part of any invocation token.
    let written = out.text();
    assert_eq!(written.lines().count(), 3);
    assert!(!written.contains(SYSTEM_TOKEN), "{written}");
    for token in &tokens {
        for part in token.split('.') {
            assert!(!written.contains(part), "{written}");
        }
    }
}

#[tokio::test]
async fn the_sink_is_handed_the_system_token_but_never_shows_it() {
    // Arrange
    let token = PUBLISHED.token();

    // Act
    let (_, accepted) = deliver(Published::keys(&[&PUBLISHED]), &token, &update()).await;

    // Assert
    let system_token = accepted[0].system_token.as_ref().expect("forwarded");
    assert_eq!(system_token.expose(), SYSTEM_TOKEN);
    let shown = format!("{:?}", accepted[0]);
    assert!(!shown.contains(SYSTEM_TOKEN), "{shown}");
    assert!(!shown.contains(&token), "{shown}");
}

#[tokio::test]
async fn a_line_break_inside_a_field_stays_inside_the_line() {
    // Arrange
    let mut event = update();
    event["payload"]["atlassianId"] = json!("557058:editor\n{\"event\":\"forged\"}\r\n");

    // Act
    let line = line_for(&event).await;

    // Assert: `line_for` already proved one line was written.
    assert_eq!(line["actor"], "557058:editor\n{\"event\":\"forged\"}\r\n");
}

#[tokio::test]
async fn a_failing_sink_defers_the_delivery_for_a_retry() {
    // Arrange
    let sink = Recorder {
        failing: true,
        ..Recorder::default()
    };
    let app = app(Published::keys(&[&PUBLISHED]), sink);

    // Act
    let answer = send(&app, delivery(&PUBLISHED.token(), &update())).await;

    // Assert
    assert_eq!(answer.status, StatusCode::SERVICE_UNAVAILABLE);
}

/// A writer whose first write blocks until the test lets it go, as standard
/// output does when nothing drains the pipe.
struct Stalled(mpsc::Receiver<()>);

impl Write for Stalled {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        // An error means the test is over and dropped its sender.
        let _ = self.0.recv();
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_stalled_writer_defers_the_delivery_instead_of_holding_it() {
    // Arrange
    let (release, stalled) = mpsc::channel();
    let app = app(
        Published::keys(&[&PUBLISHED]),
        JsonLinesSink::new(Stalled(stalled)),
    );
    let request = delivery(&PUBLISHED.token(), &update());

    // Act: the documented answer to a slow sink is a 503 within the sink
    // budget, well inside Forge's own deadline.
    let pending = tokio::spawn(async move { app.oneshot(request).await });
    let answered = tokio::time::timeout(Duration::from_secs(10), pending).await;
    drop(release);

    // Assert
    let response = answered
        .expect("the receiver answered while its writer was stalled")
        .unwrap()
        .unwrap();
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
}

// ---------------------------------------------------------------------------
// What a refusal reason can carry into the log
// ---------------------------------------------------------------------------

#[tokio::test]
async fn refusal_reasons_never_carry_control_characters() {
    // Arrange: a key ID and an algorithm come from a token nobody has
    // verified yet, and a page ID from a body; each refusal is logged as one
    // line, so none may start another.
    let verifier = FitVerifier::new(
        Trust::new(APP_ID, CLOUD_ID).unwrap(),
        Published::keys(&[&PUBLISHED]),
    );
    let forged = "\npgokf-confluence-events: delivery accepted\u{1b}[2K";
    let unknown_key = PUBLISHED.sign(
        &json!({"alg": "RS256", "kid": format!("wf-unknown{forged}")}),
        &claims(),
    );
    let strange_algorithm = PUBLISHED.sign(
        &json!({"alg": format!("RS256{forged}"), "kid": PUBLISHED.kid}),
        &claims(),
    );
    let mut event = update();
    event["payload"]["content"]["id"] = json!(format!("838205441{forged}"));
    without(&mut event["payload"]["content"], "space");

    // Act
    let reasons = [
        verifier.verify(&unknown_key).await.unwrap_err().to_string(),
        verifier
            .verify(&strange_algorithm)
            .await
            .unwrap_err()
            .to_string(),
        decode_delivery(&serde_json::to_vec(&event).unwrap())
            .unwrap_err()
            .to_string(),
    ];

    // Assert
    for reason in reasons {
        assert!(!reason.chars().any(char::is_control), "{reason:?}");
    }
}
