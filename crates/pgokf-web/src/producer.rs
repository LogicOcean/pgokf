// SPDX-License-Identifier: AGPL-3.0-only
//! Client for the repository-registry producer service's admin API, behind
//! the Admin page's Registry tab.
//!
//! The producer (a separate service, sharing this catalog's database) exposes
//! per-repository git fetch credentials over three endpoints:
//!
//! - `PUT /admin/repositories/{id}/credential` with `{label, type, secret}`
//!   creates or replaces the credential (`201`);
//! - `DELETE /admin/repositories/{id}/credential` removes it (`204`), returning
//!   the repository to anonymous fetches;
//! - `GET /admin/repositories/{id}/credential` reports `label`, `type`,
//!   `secret_last4` (null while the stored secret cannot be unsealed),
//!   `state` (`configured` or `unusable`), and `updated_at` - never the
//!   secret itself. A successful PUT's `201` body is the same document.
//!
//! Every call carries the static admin bearer token this server is configured
//! with. The token and any secret a form submits live only in this process's
//! memory for the duration of the call: nothing here writes either to the
//! database, to a log, or to a response, and error messages quote only the
//! HTTP status - never a request or response body, which a misbehaving
//! counterpart could stuff with an echoed secret.

use std::time::Duration;

use anyhow::{Context, Result};
use reqwest::{Client, StatusCode};
use serde::{Deserialize, Serialize};

use crate::routes::filters::percent_encode;

/// One bounded answer from the producer; a wedged endpoint must not hold a
/// page render or an admin action open.
const PRODUCER_TIMEOUT: Duration = Duration::from_secs(5);

/// A configured client for one producer admin API.
pub(crate) struct ProducerAdmin {
    http: Client,
    /// The base URL, without a trailing slash.
    base: String,
    /// The static admin bearer token; server-side only.
    token: String,
}

/// A repository's credential as the producer reports it (never the secret).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CredentialInfo {
    pub label: String,
    /// The credential type (`github_pat`, `http_basic`).
    pub kind: String,
    /// The secret's last four characters, or `None` while the credential is
    /// `unusable` (the producer could not unseal it - it renders the label
    /// and type with no last-four marker rather than nothing at all).
    pub secret_last4: Option<String>,
    /// `configured` or `unusable` (the stored secret no longer unseals, for
    /// example after a key rotation; the label and type still report).
    pub state: String,
    pub updated_at: String,
}

/// What a failed producer call means to the Admin page.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ProducerError {
    /// The producer did not answer (transport, timeout) or answered a 5xx:
    /// nothing was changed knowingly, and the page says "unavailable".
    Unavailable,
    /// The repository id names nothing the producer administers (404).
    UnknownRepository,
    /// The producer refused the request (a 4xx other than 404), by status.
    Rejected(StatusCode),
    /// A success status carried a body this client could not read.
    Unexpected(String),
}

impl ProducerError {
    /// The user-facing phrasing, quoted statuses included; request and
    /// response bodies are deliberately never part of it.
    pub(crate) fn message(&self) -> String {
        match self {
            Self::Unavailable => {
                "The producer admin API is unavailable; nothing was changed. Try again in a \
                 moment."
                    .to_owned()
            }
            Self::UnknownRepository => "The producer knows no repository with that id.".to_owned(),
            Self::Rejected(status) => format!(
                "The producer refused the request (HTTP {}).",
                status.as_u16()
            ),
            Self::Unexpected(why) => format!("The producer's answer was not readable: {why}."),
        }
    }

    /// Whether the failure is an outage (503 to the page) rather than a
    /// refusal of what was asked (400).
    pub(crate) fn is_unavailable(&self) -> bool {
        matches!(self, Self::Unavailable)
    }
}

/// The credential document the producer's GET returns - and a successful
/// PUT's `201` body carries the same shape. The secret is never part of it
/// by contract; the field set here makes that structural. `secret_last4` is
/// null while the stored secret cannot be unsealed (`state: "unusable"`).
#[derive(Debug, Deserialize)]
struct CredentialDocument {
    label: String,
    #[serde(rename = "type")]
    kind: String,
    secret_last4: Option<String>,
    state: String,
    updated_at: String,
}

/// The body of `PUT .../credential`. `secret` transits exactly once, into
/// this request.
#[derive(Debug, Serialize)]
struct CredentialChange<'a> {
    label: &'a str,
    #[serde(rename = "type")]
    kind: &'a str,
    secret: &'a str,
}

impl ProducerAdmin {
    /// Build a client. `base` is the producer's base URL (for example
    /// `http://producer:8081`); the `/admin/repositories/...` paths are
    /// appended here so callers configure only the base.
    pub(crate) fn new(base: &str, token: &str) -> Result<Self> {
        let http = Client::builder()
            .timeout(PRODUCER_TIMEOUT)
            .build()
            .context("failed to build the HTTP client")?;
        Ok(Self {
            http,
            base: base.trim_end_matches('/').to_owned(),
            token: token.to_owned(),
        })
    }

    /// The credential endpoint of one repository.
    fn credential_url(&self, repository_id: &str) -> String {
        format!(
            "{}/admin/repositories/{}/credential",
            self.base,
            percent_encode(repository_id)
        )
    }

    /// The credential of one repository, or `None` when none is set (the
    /// repository fetches anonymously). Never contains the secret.
    pub(crate) async fn credential(
        &self,
        repository_id: &str,
    ) -> Result<Option<CredentialInfo>, ProducerError> {
        let response = self
            .http
            .get(self.credential_url(repository_id))
            .bearer_auth(&self.token)
            .send()
            .await
            .map_err(|_| ProducerError::Unavailable)?;
        match response.status() {
            StatusCode::OK => {
                let document = response
                    .json::<CredentialDocument>()
                    .await
                    .map_err(|error| ProducerError::Unexpected(error.to_string()))?;
                Ok(Some(CredentialInfo {
                    label: document.label,
                    kind: document.kind,
                    secret_last4: document.secret_last4,
                    state: document.state,
                    updated_at: document.updated_at,
                }))
            }
            StatusCode::NOT_FOUND => Ok(None),
            status if status.is_server_error() => Err(ProducerError::Unavailable),
            status => Err(ProducerError::Rejected(status)),
        }
    }

    /// Create or replace a repository's credential. The secret leaves this
    /// process only in this request body.
    pub(crate) async fn set_credential(
        &self,
        repository_id: &str,
        label: &str,
        kind: &str,
        secret: &str,
    ) -> Result<(), ProducerError> {
        let response = self
            .http
            .put(self.credential_url(repository_id))
            .bearer_auth(&self.token)
            .json(&CredentialChange {
                label,
                kind,
                secret,
            })
            .send()
            .await
            .map_err(|_| ProducerError::Unavailable)?;
        match response.status() {
            StatusCode::CREATED => Ok(()),
            StatusCode::NOT_FOUND => Err(ProducerError::UnknownRepository),
            status if status.is_server_error() => Err(ProducerError::Unavailable),
            status => Err(ProducerError::Rejected(status)),
        }
    }

    /// Remove a repository's credential, returning it to anonymous fetches.
    /// A repository with no credential answers 404, which this reports as
    /// the clean no-op it is.
    pub(crate) async fn remove_credential(&self, repository_id: &str) -> Result<(), ProducerError> {
        let response = self
            .http
            .delete(self.credential_url(repository_id))
            .bearer_auth(&self.token)
            .send()
            .await
            .map_err(|_| ProducerError::Unavailable)?;
        match response.status() {
            // 204 removes; 404 is the clean no-op of a repository that had
            // no credential (already anonymous).
            StatusCode::NO_CONTENT | StatusCode::NOT_FOUND => Ok(()),
            status if status.is_server_error() => Err(ProducerError::Unavailable),
            status => Err(ProducerError::Rejected(status)),
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    use super::*;

    /// One captured request: the request line and the body, for assertions
    /// on what actually crossed the wire.
    #[derive(Debug)]
    pub(crate) struct Captured {
        pub(crate) head: String,
        pub(crate) body: String,
    }

    /// A mock producer: serves each `(status, body)` answer to one request,
    /// recording what it received. Answers on 127.0.0.1 only. Shared with
    /// the routes tests, which drive the real handlers against it.
    pub(crate) async fn mock_producer(
        answers: &[(&str, &str)],
    ) -> (String, tokio::task::JoinHandle<Vec<Captured>>) {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind the mock producer");
        let address = listener.local_addr().expect("the mock's address");
        let answers: Vec<(String, String)> = answers
            .iter()
            .map(|(s, b)| ((*s).to_owned(), (*b).to_owned()))
            .collect();
        let task = tokio::spawn(async move {
            let mut captured = Vec::new();
            for (status, body) in answers {
                let (mut socket, _) = listener.accept().await.expect("accept the client");
                let mut raw = Vec::new();
                let mut chunk = [0_u8; 4096];
                // Read until the headers are complete, then exactly the
                // content-length of the body.
                let mut parsed: Option<(usize, usize)> = None;
                loop {
                    let read = socket.read(&mut chunk).await.expect("read the request");
                    if read == 0 {
                        break;
                    }
                    raw.extend_from_slice(&chunk[..read]);
                    if parsed.is_none()
                        && let Some(end) = find_subslice(&raw, b"\r\n\r\n")
                    {
                        let head = String::from_utf8_lossy(&raw[..end]).to_lowercase();
                        let length = head
                            .lines()
                            .find_map(|line| line.strip_prefix("content-length:"))
                            .and_then(|v| v.trim().parse::<usize>().ok())
                            .unwrap_or(0);
                        parsed = Some((end + 4, length));
                    }
                    if let Some((start, length)) = parsed
                        && raw.len() >= start + length
                    {
                        break;
                    }
                }
                let (start, _) = parsed.expect("a complete request head");
                let head = String::from_utf8_lossy(&raw[..start]).into_owned();
                let request_body = String::from_utf8_lossy(&raw[start..]).into_owned();
                captured.push(Captured {
                    head,
                    body: request_body,
                });
                let response = format!(
                    "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                socket
                    .write_all(response.as_bytes())
                    .await
                    .expect("write the response");
            }
            captured
        });
        (format!("http://{address}"), task)
    }

    fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
        haystack
            .windows(needle.len())
            .position(|window| window == needle)
    }

    #[tokio::test]
    async fn set_credential_posts_the_secret_once_under_the_bearer_token() {
        // Arrange
        let (base, served) = mock_producer(&[("201 Created", "")]).await;
        let producer = ProducerAdmin::new(&base, "admin-token").expect("a client");
        let canary = "canary-secret-7f3a9c-never-rendered";

        // Act
        producer
            .set_credential(
                "8d2e1c4a-0000-4000-8000-0000000000aa",
                "deploy key",
                "github_pat",
                canary,
            )
            .await
            .expect("the set succeeds");
        let captured = served.await.expect("the mock captured the request");

        // Assert: the secret crossed once, in the PUT body, under the
        // static admin bearer token - and the client held nothing back.
        let [request] = captured.try_into().expect("one request");
        assert!(request.head.starts_with(
            "PUT /admin/repositories/8d2e1c4a-0000-4000-8000-0000000000aa/credential"
        ));
        assert!(
            request
                .head
                .to_lowercase()
                .contains("authorization: bearer admin-token"),
            "the static admin token authenticates the call"
        );
        assert_eq!(
            request.body,
            format!("{{\"label\":\"deploy key\",\"type\":\"github_pat\",\"secret\":\"{canary}\"}}")
        );
    }

    #[tokio::test]
    async fn credential_reports_the_metadata_and_never_the_secret() {
        // Arrange
        let (base, served) = mock_producer(&[("200 OK", CONFIGURED_FIXTURE)]).await;
        let producer = ProducerAdmin::new(&base, "admin-token").expect("a client");

        // Act
        let info = producer
            .credential("8d2e1c4a-0000-4000-8000-0000000000aa")
            .await
            .expect("the read succeeds")
            .expect("a credential is set");
        let captured = served.await.expect("the mock captured the request");

        // Assert
        assert_eq!(
            info,
            CredentialInfo {
                label: "deploy key".to_owned(),
                kind: "github_pat".to_owned(),
                secret_last4: Some("a1b2".to_owned()),
                state: "configured".to_owned(),
                updated_at: "2026-09-13T10:00:00Z".to_owned(),
            }
        );
        let [request] = captured.try_into().expect("one request");
        assert!(request.head.starts_with("GET /admin/repositories/"));
        assert!(
            request
                .head
                .to_lowercase()
                .contains("authorization: bearer admin-token")
        );
    }

    #[tokio::test]
    async fn credential_absent_is_a_clean_none_and_removing_it_a_no_op() {
        // Arrange: no credential set (GET 404), then a removal that finds
        // none either (DELETE 404 - already anonymous).
        let (base, served) = mock_producer(&[
            ("404 Not Found", "{\"error\":\"none set\"}"),
            ("404 Not Found", "{\"error\":\"none set\"}"),
        ])
        .await;
        let producer = ProducerAdmin::new(&base, "admin-token").expect("a client");

        // Act & Assert
        assert_eq!(
            producer
                .credential("8d2e1c4a-0000-4000-8000-0000000000aa")
                .await,
            Ok(None)
        );
        assert!(
            producer
                .remove_credential("8d2e1c4a-0000-4000-8000-0000000000aa")
                .await
                .is_ok(),
            "removing a credential that was never set is a clean no-op"
        );
        let captured = served.await.expect("the mock captured both requests");
        assert_eq!(captured.len(), 2);
        assert!(captured[1].head.starts_with("DELETE /admin/repositories/"));
    }

    #[tokio::test]
    async fn remove_credential_deletes_under_the_bearer_token() {
        // Arrange
        let (base, served) = mock_producer(&[("204 No Content", "")]).await;
        let producer = ProducerAdmin::new(&base, "admin-token").expect("a client");

        // Act
        producer
            .remove_credential("8d2e1c4a-0000-4000-8000-0000000000aa")
            .await
            .expect("the remove succeeds");
        let captured = served.await.expect("the mock captured the request");

        // Assert
        let [request] = captured.try_into().expect("one request");
        assert!(request.head.starts_with(
            "DELETE /admin/repositories/8d2e1c4a-0000-4000-8000-0000000000aa/credential"
        ));
        assert!(
            request
                .head
                .to_lowercase()
                .contains("authorization: bearer admin-token")
        );
    }

    #[tokio::test]
    async fn an_unreachable_producer_is_unavailable_not_a_panic() {
        // Arrange: a port nothing listens on.
        let producer = ProducerAdmin::new("http://127.0.0.1:9", "admin-token").expect("a client");

        // Act & Assert
        let set = producer
            .set_credential("id", "label", "github_pat", "canary-secret")
            .await
            .expect_err("a refused connection is an error");
        let read = producer
            .credential("id")
            .await
            .expect_err("a refused connection is an error");
        assert_eq!(set, ProducerError::Unavailable);
        assert_eq!(read, ProducerError::Unavailable);
        assert!(set.is_unavailable());
        assert_eq!(
            set.message(),
            "The producer admin API is unavailable; nothing was changed. Try again in a moment."
        );
    }

    #[tokio::test]
    async fn an_error_answer_never_echoes_the_secret_it_was_sent() {
        // Arrange: a producer that fails the request and - as misbehaving
        // frameworks do - echoes the whole request body, secret included.
        let canary = "canary-secret-7f3a9c-never-rendered";
        let echoed = format!("{{\"error\":\"rejected\",\"you_sent\":{{\"secret\":\"{canary}\"}}}}");
        let (base, served) = mock_producer(&[("400 Bad Request", &echoed)]).await;
        let producer = ProducerAdmin::new(&base, "admin-token").expect("a client");

        // Act
        let error = producer
            .set_credential("id", "label", "github_pat", canary)
            .await
            .expect_err("a 400 is an error");
        let _ = served.await;

        // Assert: the message an admin page would render quotes the status
        // alone, so the echoed canary cannot ride it into the UI or a log.
        assert_eq!(error, ProducerError::Rejected(StatusCode::BAD_REQUEST));
        assert!(!error.message().contains(canary));
        assert_eq!(
            error.message(),
            "The producer refused the request (HTTP 400)."
        );
    }

    // ------------------------------------------------------------------
    // Cross-repo contract
    // ------------------------------------------------------------------

    /// The two fixtures below are verbatim in shape from the producer's
    /// `CredentialInfoResponse`
    /// (`/tmp/registry-ui/src/ast_graph/producer/admin.py`): keys `label`,
    /// `type`, `secret_last4` (nullable), `state` (`configured` |
    /// `unusable`), `updated_at`. The producer repo mirrors this test on its
    /// side; a drift in the document shape must fail a test here or there.
    const CONFIGURED_FIXTURE: &str = "{\"label\":\"deploy key\",\"type\":\"github_pat\",\"secret_last4\":\"a1b2\",\"state\":\"configured\",\"updated_at\":\"2026-09-13T10:00:00Z\"}";
    const UNUSABLE_FIXTURE: &str = "{\"label\":\"legacy key\",\"type\":\"http_basic\",\"secret_last4\":null,\"state\":\"unusable\",\"updated_at\":\"2026-09-01T09:00:00Z\"}";

    /// The web half of the credential-document contract: both documented
    /// shapes parse, and every status the producer's admin API defines lands
    /// where the Admin page expects it (`201` PUT body parses as the same
    /// document; `404` GET is `None`; `204` DELETE is `Ok`; `400` is a
    /// typed rejection). The render half lives in routes.rs
    /// (`the_producer_contract_renders_both_credential_states`).
    #[tokio::test]
    async fn the_producer_credential_contract_deserializes_and_maps_statuses() {
        // Both fixtures deserialize, including the null last-four of an
        // unusable credential.
        let configured: CredentialDocument =
            serde_json::from_str(CONFIGURED_FIXTURE).expect("the configured fixture parses");
        assert_eq!(configured.state, "configured");
        assert_eq!(configured.secret_last4.as_deref(), Some("a1b2"));
        let unusable: CredentialDocument =
            serde_json::from_str(UNUSABLE_FIXTURE).expect("the unusable fixture parses");
        assert_eq!(unusable.state, "unusable");
        assert_eq!(unusable.secret_last4, None);

        // Arrange: a mock answering, in order, a GET (200, unusable), a PUT
        // (201, body in the same document shape), a DELETE (204), a GET of
        // nothing (404), and a refused PUT (400).
        let (base, served) = mock_producer(&[
            ("200 OK", UNUSABLE_FIXTURE),
            ("201 Created", CONFIGURED_FIXTURE),
            ("204 No Content", ""),
            (
                "404 Not Found",
                "{\"detail\":\"no credential for repository\"}",
            ),
            (
                "400 Bad Request",
                "{\"detail\":\"ssh_key credentials are not supported yet\"}",
            ),
        ])
        .await;
        let producer = ProducerAdmin::new(&base, "admin-token").expect("a client");
        let id = "8d2e1c4a-0000-4000-8000-0000000000aa";

        // Act & Assert: each status maps the way the contract fixes it.
        let info = producer
            .credential(id)
            .await
            .expect("the read succeeds")
            .expect("a credential is set");
        assert_eq!(info.state, "unusable");
        assert_eq!(info.secret_last4, None);
        producer
            .set_credential(id, "deploy key", "github_pat", "a-secret")
            .await
            .expect("201 accepts");
        producer.remove_credential(id).await.expect("204 removes");
        assert_eq!(producer.credential(id).await, Ok(None), "404 is None");
        assert_eq!(
            producer
                .set_credential(id, "key", "ssh_key", "a-secret")
                .await,
            Err(ProducerError::Rejected(StatusCode::BAD_REQUEST)),
            "400 is a typed rejection"
        );
        let captured = served.await.expect("the mock captured the requests");
        assert_eq!(captured.len(), 5);
    }
}
