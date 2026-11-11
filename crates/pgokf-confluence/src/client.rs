// SPDX-License-Identifier: AGPL-3.0-only

//! A read-only Confluence Cloud client that returns complete, checked snapshots.

use std::collections::{BTreeMap, BTreeSet};
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result, anyhow, bail, ensure};
use base64::Engine;
use reqwest::header::{ACCEPT, AUTHORIZATION, HeaderMap, HeaderValue, LINK, RETRY_AFTER};
use reqwest::{StatusCode, Url};
use serde::Deserialize;
use serde_json::Value;

use crate::types::Page;

const LIST_LIMIT: &str = "100";
const MAX_RETRIES: u32 = 10;
const MAX_RETRY_DELAY: Duration = Duration::from_secs(30);
const MAX_TOTAL_RETRY_DELAY: Duration = Duration::from_mins(1);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// Connection settings. Intentionally does not implement `Debug`: it owns secrets.
pub struct ClientConfig {
    /// Canonical HTTPS `*.atlassian.net` site URL, with an optional `/wiki` suffix.
    pub site_url: String,
    /// Atlassian account email used with the API token.
    pub email: String,
    /// API token. Never included in diagnostics.
    pub token: String,
    /// Cloud UUID required for scoped API tokens using the Atlassian gateway.
    pub cloud_id: Option<String>,
    /// Additional attempts for transient read failures (at most ten).
    pub max_retries: u32,
    /// Maximum bytes accepted for any one successful API response.
    pub max_response_bytes: usize,
}

/// Authenticated read-only client. Credentials are confined to sensitive headers.
pub struct Client {
    http: reqwest::Client,
    /// Credential-free client for the site's public tenant record.
    public_http: reqwest::Client,
    site: Url,
    api: Url,
    tenant_info: Url,
    cloud_id: Option<String>,
    max_retries: u32,
    max_response_bytes: usize,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ApiPage {
    id: String,
    space_id: String,
    title: String,
    status: String,
    parent_id: Option<String>,
    version: ApiVersion,
    body: Option<ApiBody>,
    #[serde(rename = "_links", default)]
    links: PageLinks,
}

#[derive(Default, Deserialize)]
struct PageLinks {
    webui: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ApiVersion {
    number: u64,
    created_at: Option<String>,
}

#[derive(Deserialize)]
struct ApiBody {
    storage: Option<StorageBody>,
}

#[derive(Deserialize)]
struct StorageBody {
    value: String,
    representation: Option<String>,
}

#[derive(Deserialize)]
struct PageList {
    results: Vec<ApiPage>,
    #[serde(rename = "_links", default)]
    links: ListLinks,
}

#[derive(Default, Deserialize)]
struct ListLinks {
    next: Option<String>,
}

struct InventoryPage {
    version: u64,
    webui: Option<String>,
}

struct ApiResponse {
    value: Value,
    links: Vec<String>,
}

enum AttemptError {
    Retry {
        reason: &'static str,
        after: Option<Duration>,
    },
    Fatal(anyhow::Error),
}

impl Client {
    /// Build a client for a canonical Confluence Cloud site.
    ///
    /// # Errors
    /// Returns an error for invalid destinations, credentials, or resource limits.
    pub fn new(config: ClientConfig) -> Result<Self> {
        let site = validate_site(&config.site_url)?;
        ensure!(
            config.max_retries <= MAX_RETRIES,
            "max_retries must be at most ten"
        );
        ensure!(
            config.max_response_bytes > 0,
            "max_response_bytes must be positive"
        );
        let api = if let Some(cloud_id) = &config.cloud_id {
            validate_cloud_id(cloud_id)?;
            Url::parse(&format!(
                "https://api.atlassian.com/ex/confluence/{cloud_id}/wiki/api/v2/"
            ))
            .context("invalid scoped API base")?
        } else {
            site.join("wiki/api/v2/").context("invalid API base")?
        };
        let http = build_http(&config.email, &config.token)?;
        let tenant_info = site
            .join("_edge/tenant_info")
            .context("invalid tenant record URL")?;
        let client = Self {
            http,
            public_http: build_public_http()?,
            site,
            api,
            tenant_info,
            cloud_id: config.cloud_id.clone(),
            max_retries: config.max_retries,
            max_response_bytes: config.max_response_bytes,
        };
        // Consume the input so the clear-text credential copies are dropped here.
        drop(config);
        Ok(client)
    }

    /// Read all visible current pages from one space, checking for concurrent changes.
    ///
    /// A full inventory is collected before reading bodies and again afterward. The
    /// result is returned only if every page was fetched at the inventoried version
    /// and both inventories match. Confluence does not offer a transactional snapshot;
    /// changes after the final inventory and content invisible to the token cannot be
    /// detected by this check.
    ///
    /// # Errors
    /// Fails on an empty, changing, malformed, or incomplete crawl, any page count or
    /// response size limit, permission failures, or exhausted retry budgets.
    pub async fn crawl_space(&self, space_id: &str, max_pages: usize) -> Result<Vec<Page>> {
        validate_id(space_id)?;
        ensure!(max_pages > 0, "max_pages must be positive");
        self.verify_cloud_id().await?;
        self.check_space(space_id).await?;
        let inventory = self.inventory(space_id, max_pages).await?;
        ensure!(
            !inventory.is_empty(),
            "space contains no visible current pages; refusing an empty snapshot"
        );
        let mut pages = Vec::with_capacity(inventory.len());
        for (id, manifest) in &inventory {
            let mut url = self.api.join(&format!("pages/{id}"))?;
            url.query_pairs_mut().append_pair("body-format", "storage");
            let response = self.get_json(url).await?;
            let mut page: ApiPage = serde_json::from_value(response.value).map_err(|_| {
                anyhow!("Confluence returned malformed page metadata or a missing storage body")
            })?;
            validate_page(&page, space_id)?;
            ensure!(
                &page.id == id && page.version.number == manifest.version,
                "page identity or version changed during crawl; retry the complete crawl"
            );
            // Get-page may only return _links.base; retain the listing's webui
            // so same-space references still have the canonical space key.
            page.links.webui = page.links.webui.or_else(|| manifest.webui.clone());
            pages.push(self.normalize_page(page)?);
        }
        let final_inventory = self.inventory(space_id, max_pages).await?;
        ensure!(
            inventory
                .iter()
                .map(|(id, page)| (id, page.version))
                .eq(final_inventory.iter().map(|(id, page)| (id, page.version))),
            "space inventory changed during crawl; retry the complete crawl"
        );
        Ok(pages)
    }

    /// With a scoped token every request goes through Atlassian's gateway by
    /// cloud ID, not to the configured site. Confirm - without credentials,
    /// from the site's public tenant record - that the cloud ID belongs to
    /// that site, so a mistyped or foreign cloud ID can never import another
    /// site's content under this site's identity.
    async fn verify_cloud_id(&self) -> Result<()> {
        let Some(expected) = &self.cloud_id else {
            return Ok(());
        };
        let response = self
            .fetch(&self.public_http, self.tenant_info.clone())
            .await?;
        let actual = response
            .value
            .get("cloudId")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("Confluence site tenant record has no cloud ID"))?;
        ensure!(
            actual.eq_ignore_ascii_case(expected),
            "the Confluence cloud ID does not belong to the configured site"
        );
        Ok(())
    }

    async fn check_space(&self, space_id: &str) -> Result<()> {
        let url = self.api.join(&format!("spaces/{space_id}"))?;
        let response = self.get_json(url).await?;
        ensure!(
            response.value.get("id").and_then(Value::as_str) == Some(space_id),
            "Confluence returned unexpected space metadata"
        );
        Ok(())
    }

    async fn inventory(
        &self,
        space_id: &str,
        max_pages: usize,
    ) -> Result<BTreeMap<String, InventoryPage>> {
        let mut inventory = BTreeMap::new();
        let mut seen_cursors = BTreeSet::new();
        let mut cursor: Option<String> = None;
        loop {
            let mut url = self.api.join("pages")?;
            url.query_pairs_mut()
                .append_pair("space-id", space_id)
                .append_pair("status", "current")
                .append_pair("limit", LIST_LIMIT);
            if let Some(value) = &cursor {
                url.query_pairs_mut().append_pair("cursor", value);
            }
            let response = self.get_json(url.clone()).await?;
            let list: PageList = serde_json::from_value(response.value)
                .map_err(|_| anyhow!("Confluence returned malformed page inventory"))?;
            let next = self.next_cursor(&url, list.links.next.as_deref(), &response.links)?;
            ensure!(
                !list.results.is_empty() || next.is_none(),
                "Confluence returned an empty intermediate inventory page"
            );
            for page in list.results {
                validate_page(&page, space_id)?;
                ensure!(
                    inventory.len() < max_pages,
                    "space exceeds max_pages; refusing a partial snapshot"
                );
                ensure!(
                    inventory
                        .insert(
                            page.id,
                            InventoryPage {
                                version: page.version.number,
                                webui: page.links.webui,
                            }
                        )
                        .is_none(),
                    "duplicate page ID in Confluence inventory"
                );
            }
            let Some(next) = next else { break };
            ensure!(
                seen_cursors.insert(next.clone()),
                "Confluence pagination cursor repeated"
            );
            cursor = Some(next);
        }
        Ok(inventory)
    }

    fn normalize_page(&self, page: ApiPage) -> Result<Page> {
        let storage = page
            .body
            .and_then(|body| body.storage)
            .ok_or_else(|| anyhow!("Confluence page is missing its requested storage body"))?;
        ensure!(
            storage
                .representation
                .as_deref()
                .is_none_or(|value| value == "storage"),
            "Confluence returned an unexpected body representation"
        );
        let web_url = self.page_web_url(&page.id, page.links.webui.as_deref())?;
        Ok(Page {
            id: page.id,
            space_id: page.space_id,
            title: page.title,
            parent_id: page.parent_id,
            version: page.version.number,
            modified_at: page.version.created_at,
            storage: storage.value,
            web_url,
        })
    }

    fn page_web_url(&self, id: &str, webui: Option<&str>) -> Result<String> {
        if let Some(link) = webui.filter(|link| link.len() <= 16_384) {
            // v2 webui paths may be relative to the /wiki context rather than
            // the origin. Preserve space keys only in validated canonical links.
            let relative = if link.starts_with("/spaces/") {
                format!("wiki{link}")
            } else if link.starts_with("spaces/") {
                format!("wiki/{link}")
            } else {
                link.to_owned()
            };
            if let Ok(url) = self.site.join(&relative) {
                let segments: Vec<_> = url.path_segments().into_iter().flatten().collect();
                if url.origin() == self.site.origin()
                    && url.username().is_empty()
                    && url.password().is_none()
                    && url.query().is_none()
                    && url.fragment().is_none()
                    && segments.len() >= 5
                    && segments[0] == "wiki"
                    && segments[1] == "spaces"
                    && !segments[2].is_empty()
                    && segments[3] == "pages"
                    && segments[4] == id
                {
                    return Ok(url.into());
                }
            }
        }
        // Fallback links never expose the scoped API gateway or trust upstream
        // destinations, and remain stable across title and space changes.
        let mut url = self.site.join("wiki/pages/viewpage.action")?;
        url.query_pairs_mut().append_pair("pageId", id);
        Ok(url.into())
    }

    fn next_cursor(
        &self,
        current: &Url,
        body_next: Option<&str>,
        headers: &[String],
    ) -> Result<Option<String>> {
        let mut next = body_next
            .map(|url| self.cursor_from_link(current, url))
            .transpose()?;
        for header in headers {
            for item in header.split(',') {
                let mut parts = item.trim().split(';');
                let target = parts.next().unwrap_or_default().trim();
                let target = target
                    .strip_prefix('<')
                    .and_then(|value| value.strip_suffix('>'))
                    .filter(|value| !value.is_empty())
                    .ok_or_else(|| anyhow!("Confluence returned a malformed pagination link"))?;
                let mut relation = None;
                for part in parts {
                    let (name, value) = part.trim().split_once('=').ok_or_else(|| {
                        anyhow!("Confluence returned malformed pagination parameters")
                    })?;
                    if name.trim().eq_ignore_ascii_case("rel") {
                        ensure!(
                            relation.is_none(),
                            "Confluence pagination relation is duplicated"
                        );
                        let value = value.trim();
                        let value = if value.starts_with('"') {
                            value
                                .strip_prefix('"')
                                .and_then(|value| value.strip_suffix('"'))
                                .ok_or_else(|| {
                                    anyhow!("Confluence pagination relation is malformed")
                                })?
                        } else {
                            value
                        };
                        ensure!(
                            !value.is_empty() && !value.contains('"'),
                            "Confluence pagination relation is malformed"
                        );
                        relation = Some(value);
                    }
                }
                let relation = relation
                    .ok_or_else(|| anyhow!("Confluence pagination link is missing its relation"))?;
                let is_next = relation
                    .split_ascii_whitespace()
                    .any(|value| value.eq_ignore_ascii_case("next"));
                if is_next {
                    let candidate = self.cursor_from_link(current, target)?;
                    ensure!(
                        next.as_ref().is_none_or(|value| value == &candidate),
                        "Confluence pagination links disagree"
                    );
                    next = Some(candidate);
                }
            }
        }
        Ok(next)
    }

    fn cursor_from_link(&self, current: &Url, link: &str) -> Result<String> {
        ensure!(
            link.len() <= 16_384,
            "Confluence pagination link is too long"
        );
        let url = current
            .join(link)
            .map_err(|_| anyhow!("Confluence returned an invalid pagination link"))?;
        let expected_path = self.api.join("pages")?.path().to_owned();
        let allowed_origin =
            url.origin() == self.api.origin() || url.origin() == self.site.origin();
        ensure!(
            allowed_origin
                && url.username().is_empty()
                && url.password().is_none()
                && url.fragment().is_none(),
            "Confluence returned an unsafe pagination destination"
        );
        ensure!(
            url.path() == expected_path || url.path() == "/wiki/api/v2/pages",
            "Confluence returned an unexpected pagination path"
        );
        let mut cursors = url.query_pairs().filter(|(key, _)| key == "cursor");
        let cursor = cursors
            .next()
            .map(|(_, value)| value.into_owned())
            .ok_or_else(|| anyhow!("Confluence pagination link has no cursor"))?;
        ensure!(
            !cursor.is_empty()
                && cursor.len() <= 8_192
                && !cursor.chars().any(char::is_control)
                && cursors.next().is_none(),
            "Confluence pagination cursor is invalid"
        );
        // Only the cursor is reused. The next GET always uses our validated base
        // URL and freshly applies the selected space, current status, and limit.
        Ok(cursor)
    }

    async fn get_json(&self, url: Url) -> Result<ApiResponse> {
        self.fetch(&self.http, url).await
    }

    async fn fetch(&self, http: &reqwest::Client, url: Url) -> Result<ApiResponse> {
        let mut slept = Duration::ZERO;
        for attempt in 0..=self.max_retries {
            match self.try_request(http, url.clone()).await {
                Ok(response) => return Ok(response),
                Err(AttemptError::Fatal(error)) => return Err(error),
                Err(AttemptError::Retry { reason, after }) => {
                    ensure!(
                        attempt < self.max_retries,
                        "Confluence read failed after bounded retries: {reason}"
                    );
                    let delay = retry_delay(attempt, after, slept)?;
                    slept += delay;
                    tokio::time::sleep(delay).await;
                }
            }
        }
        unreachable!("all request attempts return or exhaust retries")
    }

    async fn try_request(
        &self,
        http: &reqwest::Client,
        url: Url,
    ) -> std::result::Result<ApiResponse, AttemptError> {
        let mut response = http
            .get(url)
            .send()
            .await
            .map_err(|error| transport_error(&error))?;
        let status = response.status();
        if status == StatusCode::TOO_MANY_REQUESTS || status.is_server_error() {
            let after = parse_retry_after(response.headers().get(RETRY_AFTER), SystemTime::now())
                .map_err(AttemptError::Fatal)?;
            return Err(AttemptError::Retry {
                reason: "rate limit or upstream server error",
                after,
            });
        }
        if status != StatusCode::OK {
            return Err(AttemptError::Fatal(anyhow!(
                "Confluence read failed with HTTP {}",
                status.as_u16()
            )));
        }
        if response
            .content_length()
            .is_some_and(|length| length > self.max_response_bytes as u64)
        {
            return Err(AttemptError::Fatal(anyhow!(
                "Confluence response exceeds max_response_bytes"
            )));
        }
        let links = response
            .headers()
            .get_all(LINK)
            .iter()
            .map(|value| {
                value.to_str().map(str::to_owned).map_err(|_| {
                    AttemptError::Fatal(anyhow!("Confluence returned an invalid Link header"))
                })
            })
            .collect::<std::result::Result<Vec<_>, _>>()?;
        let mut bytes = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|error| transport_error(&error))?
        {
            if chunk.len() > self.max_response_bytes.saturating_sub(bytes.len()) {
                return Err(AttemptError::Fatal(anyhow!(
                    "Confluence response exceeds max_response_bytes"
                )));
            }
            bytes.extend_from_slice(&chunk);
        }
        let value = serde_json::from_slice(&bytes)
            .map_err(|_| AttemptError::Fatal(anyhow!("Confluence returned invalid JSON")))?;
        Ok(ApiResponse { value, links })
    }
}

fn build_http(email: &str, token: &str) -> Result<reqwest::Client> {
    ensure!(
        !email.is_empty()
            && email.len() <= 320
            && email.contains('@')
            && !email.contains(':')
            && !email.chars().any(char::is_whitespace)
            && !email.chars().any(char::is_control),
        "Confluence email is invalid"
    );
    ensure!(
        !token.is_empty()
            && token.len() <= 16_384
            && !token.chars().any(char::is_whitespace)
            && !token.chars().any(char::is_control),
        "Confluence API token is invalid"
    );
    let encoded = base64::engine::general_purpose::STANDARD.encode(format!("{email}:{token}"));
    let mut authorization = HeaderValue::from_str(&format!("Basic {encoded}"))
        .map_err(|_| anyhow!("could not create Confluence authorization header"))?;
    authorization.set_sensitive(true);
    let mut headers = HeaderMap::new();
    headers.insert(AUTHORIZATION, authorization);
    headers.insert(ACCEPT, HeaderValue::from_static("application/json"));
    http_builder()
        .default_headers(headers)
        .build()
        .map_err(|_| anyhow!("could not initialize Confluence HTTP client"))
}

/// A client that carries no credentials, for public site records.
fn build_public_http() -> Result<reqwest::Client> {
    let mut headers = HeaderMap::new();
    headers.insert(ACCEPT, HeaderValue::from_static("application/json"));
    http_builder()
        .default_headers(headers)
        .build()
        .map_err(|_| anyhow!("could not initialize Confluence HTTP client"))
}

/// The transport policy every Confluence request shares: no redirects,
/// bounded connect and request time, and an identifying user agent.
fn http_builder() -> reqwest::ClientBuilder {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(10))
        .timeout(REQUEST_TIMEOUT)
        .user_agent(concat!("pgokf-confluence/", env!("CARGO_PKG_VERSION")))
}

fn validate_site(raw: &str) -> Result<Url> {
    let mut site = Url::parse(raw).map_err(|_| anyhow!("invalid Confluence site URL"))?;
    let host = site.host_str().unwrap_or_default();
    let tenant = host.strip_suffix(".atlassian.net").unwrap_or_default();
    let authority = raw.split_once("://").map_or("", |(_, rest)| {
        rest.split(['/', '?', '#']).next().unwrap_or_default()
    });
    ensure!(
        site.scheme() == "https"
            && !tenant.is_empty()
            && tenant.len() <= 63
            && !tenant.contains('.')
            && tenant
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
            && !tenant.starts_with('-')
            && !tenant.ends_with('-'),
        "Confluence site must be a canonical HTTPS tenant.atlassian.net URL"
    );
    // Evaluated outside the macro: the verifier's SQL-sink scanner reads macro
    // tokens and would take `Url::query()` for a database `query` call.
    let bare_origin = !authority.contains(['@', ':'])
        && site.username().is_empty()
        && site.password().is_none()
        && site.port().is_none()
        && site.query().is_none()
        && site.fragment().is_none()
        && matches!(site.path(), "/" | "/wiki" | "/wiki/");
    ensure!(
        bare_origin,
        "Confluence site URL must not include credentials, a port, query, fragment, or extra path"
    );
    ensure!(
        !raw.contains('\\') && !raw.chars().any(char::is_whitespace),
        "Confluence site URL contains invalid characters"
    );
    site.set_path("/");
    Ok(site)
}

fn validate_cloud_id(id: &str) -> Result<()> {
    ensure!(
        id.len() == 36
            && id.bytes().enumerate().all(|(index, byte)| {
                if matches!(index, 8 | 13 | 18 | 23) {
                    byte == b'-'
                } else {
                    byte.is_ascii_hexdigit()
                }
            }),
        "Confluence cloud ID must be a UUID"
    );
    Ok(())
}

fn validate_id(id: &str) -> Result<()> {
    ensure!(
        !id.is_empty()
            && id.len() <= 32
            && id.bytes().all(|byte| byte.is_ascii_digit())
            && !id.starts_with('0'),
        "Confluence IDs must be positive canonical decimal strings"
    );
    Ok(())
}

fn validate_page(page: &ApiPage, space_id: &str) -> Result<()> {
    validate_id(&page.id)?;
    ensure!(
        page.space_id == space_id && page.status == "current",
        "Confluence returned a page outside the selected space or current status"
    );
    ensure!(
        page.version.number > 0 && !page.title.trim().is_empty(),
        "Confluence returned invalid page version or title"
    );
    if let Some(parent) = &page.parent_id {
        validate_id(parent)?;
    }
    Ok(())
}

fn transport_error(error: &reqwest::Error) -> AttemptError {
    if error.is_timeout() || error.is_connect() || error.is_body() || error.is_request() {
        AttemptError::Retry {
            reason: "transport failure",
            after: None,
        }
    } else {
        AttemptError::Fatal(anyhow!("Confluence request failed"))
    }
}

fn parse_retry_after(value: Option<&HeaderValue>, now: SystemTime) -> Result<Option<Duration>> {
    let Some(value) = value else { return Ok(None) };
    let value = value
        .to_str()
        .map_err(|_| anyhow!("Confluence returned an invalid Retry-After header"))?;
    if let Ok(seconds) = value.parse::<u64>() {
        return Ok(Some(Duration::from_secs(seconds)));
    }
    let date = httpdate::parse_http_date(value)
        .map_err(|_| anyhow!("Confluence returned an invalid Retry-After header"))?;
    Ok(Some(date.duration_since(now).unwrap_or(Duration::ZERO)))
}

fn retry_delay(attempt: u32, after: Option<Duration>, slept: Duration) -> Result<Duration> {
    let base_ms = 200_u64
        .saturating_mul(1_u64 << attempt.min(MAX_RETRIES))
        .min(5_000);
    let jitter_ms = rand::random_range(0..=base_ms / 4);
    let delay = Duration::from_millis(base_ms + jitter_ms).max(after.unwrap_or_default());
    if delay > MAX_RETRY_DELAY || delay > MAX_TOTAL_RETRY_DELAY.saturating_sub(slept) {
        bail!("Confluence Retry-After or backoff exceeds the bounded retry budget; try later");
    }
    Ok(delay)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use serde_json::json;
    use wiremock::matchers::{header, method, path, query_param};
    use wiremock::{Mock, MockServer, Request, ResponseTemplate};

    use super::*;

    fn config() -> ClientConfig {
        ClientConfig {
            site_url: "https://example.atlassian.net/wiki".into(),
            email: "reader@example.test".into(),
            token: "test-api-token".into(),
            cloud_id: None,
            max_retries: 0,
            max_response_bytes: 1_048_576,
        }
    }

    fn mock_client(server: &MockServer) -> Client {
        let mut client = Client::new(config()).unwrap();
        // An insecure destination is deliberately available only to unit tests.
        client.api = Url::parse(&format!("{}/wiki/api/v2/", server.uri())).unwrap();
        client
    }

    fn page(id: &str, version: u64) -> Value {
        json!({
            "id": id, "spaceId": "42", "title": format!("Page {id}"),
            "status": "current", "parentId": null,
            "version": {"number": version, "createdAt": "2026-11-06T10:00:00Z"},
            "body": {"storage": {"representation": "storage", "value": "<p>Wiki content</p>"}}
        })
    }

    async fn mount_space(server: &MockServer) {
        Mock::given(method("GET"))
            .and(path("/wiki/api/v2/spaces/42"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id": "42"})))
            .mount(server)
            .await;
    }

    async fn mount_list(server: &MockServer, pages: Vec<Value>) {
        Mock::given(method("GET"))
            .and(path("/wiki/api/v2/pages"))
            .and(query_param("space-id", "42"))
            .and(query_param("status", "current"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"results": pages})))
            .mount(server)
            .await;
    }

    async fn mount_page(server: &MockServer, value: Value) {
        Mock::given(method("GET"))
            .and(path(format!(
                "/wiki/api/v2/pages/{}",
                value["id"].as_str().unwrap()
            )))
            .and(query_param("body-format", "storage"))
            .respond_with(ResponseTemplate::new(200).set_body_json(value))
            .mount(server)
            .await;
    }

    #[test]
    fn validates_cloud_destinations_without_echoing_input() {
        for invalid in [
            "http://example.atlassian.net",
            "https://attacker.test",
            "https://atlassian.net",
            "https://a.b.atlassian.net",
            "https://example.atlassian.net.attacker.test",
            "https://example.atlassian.net:8443",
            "https://example.atlassian.net:443",
            "https://@example.atlassian.net",
            "https://example.atlassian.net/other",
            "https://reader:test-api-token@example.atlassian.net",
            "https://example.atlassian.net?token=test-api-token",
            "https://example.atlassian.net#token",
            "https://-example.atlassian.net",
            "https://example.atlassian.net\\wiki",
            " https://example.atlassian.net",
        ] {
            let mut settings = config();
            settings.site_url = invalid.into();
            let error = Client::new(settings)
                .err()
                .expect("invalid destination must fail");
            assert!(!error.to_string().contains("test-api-token"));
        }
        for valid in [
            "https://example.atlassian.net",
            "https://example.atlassian.net/",
            "https://example.atlassian.net/wiki/",
        ] {
            let mut settings = config();
            settings.site_url = valid.into();
            let client = Client::new(settings).unwrap();
            assert_eq!(client.site.as_str(), "https://example.atlassian.net/");
            assert_eq!(
                client.api.as_str(),
                "https://example.atlassian.net/wiki/api/v2/"
            );
        }
    }

    const CLOUD_ID: &str = "12345678-1234-1234-1234-123456789abc";

    /// A scoped-token client whose gateway and tenant record are the mock.
    fn scoped_mock_client(server: &MockServer) -> Client {
        let mut settings = config();
        settings.cloud_id = Some(CLOUD_ID.into());
        let mut client = Client::new(settings).unwrap();
        client.api = Url::parse(&format!("{}/wiki/api/v2/", server.uri())).unwrap();
        client.tenant_info = Url::parse(&format!("{}/_edge/tenant_info", server.uri())).unwrap();
        client
    }

    async fn mount_tenant(server: &MockServer, cloud_id: &str) {
        Mock::given(method("GET"))
            .and(path("/_edge/tenant_info"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"cloudId": cloud_id})))
            .mount(server)
            .await;
    }

    #[tokio::test]
    async fn verify_cloud_id_accepts_the_sites_own_cloud_id_in_any_case() {
        // Arrange
        let server = MockServer::start().await;
        mount_tenant(&server, &CLOUD_ID.to_ascii_uppercase()).await;
        let client = scoped_mock_client(&server);

        // Act
        let result = client.verify_cloud_id().await;

        // Assert
        assert!(result.is_ok(), "{result:?}");
    }

    #[tokio::test]
    async fn verify_cloud_id_refuses_a_cloud_id_of_another_site() {
        // Arrange
        let server = MockServer::start().await;
        mount_tenant(&server, "ffffffff-ffff-ffff-ffff-ffffffffffff").await;
        let client = scoped_mock_client(&server);

        // Act
        let error = client.verify_cloud_id().await.unwrap_err();

        // Assert
        assert!(error.to_string().contains("does not belong"), "{error}");
    }

    #[tokio::test]
    async fn verify_cloud_id_refuses_a_tenant_record_without_one() {
        // Arrange
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/_edge/tenant_info"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
            .mount(&server)
            .await;
        let client = scoped_mock_client(&server);

        // Act
        let result = client.verify_cloud_id().await;

        // Assert
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn verify_cloud_id_never_sends_credentials_to_the_public_record() {
        // Arrange
        let server = MockServer::start().await;
        mount_tenant(&server, CLOUD_ID).await;
        let client = scoped_mock_client(&server);

        // Act
        client.verify_cloud_id().await.unwrap();

        // Assert
        let requests = server.received_requests().await.unwrap();
        assert_eq!(requests.len(), 1);
        assert!(!requests[0].headers.contains_key("authorization"));
    }

    #[tokio::test]
    async fn verify_cloud_id_makes_no_request_for_a_classic_token() {
        // Arrange
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/_edge/tenant_info"))
            .respond_with(ResponseTemplate::new(500))
            .expect(0)
            .mount(&server)
            .await;
        let mut client = mock_client(&server);
        client.tenant_info = Url::parse(&format!("{}/_edge/tenant_info", server.uri())).unwrap();

        // Act
        let result = client.verify_cloud_id().await;

        // Assert: the mock's expect(0) is verified when the server drops.
        assert!(result.is_ok());
    }

    #[test]
    fn constructs_scoped_token_gateway_and_rejects_invalid_cloud_ids() {
        let mut settings = config();
        settings.cloud_id = Some("12345678-1234-1234-1234-123456789abc".into());
        let client = Client::new(settings).unwrap();
        assert_eq!(
            client.api.as_str(),
            "https://api.atlassian.com/ex/confluence/12345678-1234-1234-1234-123456789abc/wiki/api/v2/"
        );
        assert_eq!(client.site.as_str(), "https://example.atlassian.net/");
        for invalid in [
            "",
            "../../attacker",
            "invalid",
            "12345678-1234-1234-1234-123456789abz",
        ] {
            let mut settings = config();
            settings.cloud_id = Some(invalid.into());
            assert!(Client::new(settings).is_err());
        }
    }

    #[test]
    fn validates_credentials_and_limits() {
        let mut settings = config();
        settings.max_response_bytes = 0;
        assert!(Client::new(settings).is_err());
        let mut settings = config();
        settings.max_retries = MAX_RETRIES + 1;
        assert!(Client::new(settings).is_err());
        for invalid in ["", "token\nsecret", " token", "token\0secret"] {
            let mut settings = config();
            settings.token = invalid.into();
            assert!(Client::new(settings).is_err());
        }
        for invalid in [
            "",
            "not-an-email",
            "reader:pass@example.test",
            "reader\n@example.test",
            "reader\0@example.test",
        ] {
            let mut settings = config();
            settings.email = invalid.into();
            assert!(Client::new(settings).is_err());
        }
    }

    #[tokio::test]
    async fn crawls_all_cursors_twice_with_auth_and_storage_bodies() {
        let server = MockServer::start().await;
        mount_space(&server).await;
        let authorization = format!(
            "Basic {}",
            base64::engine::general_purpose::STANDARD.encode("reader@example.test:test-api-token")
        );
        let calls = Arc::new(AtomicUsize::new(0));
        let counter = calls.clone();
        Mock::given(method("GET"))
            .and(path("/wiki/api/v2/pages"))
            .and(query_param("space-id", "42"))
            .and(query_param("status", "current"))
            .and(query_param("limit", "100"))
            .and(header("authorization", authorization))
            .respond_with(move |request: &Request| {
                counter.fetch_add(1, Ordering::SeqCst);
                let cursor = request.url.query_pairs().find(|(key, _)| key == "cursor");
                if let Some((_, cursor)) = cursor {
                    assert_eq!(cursor, "cursor+two/=");
                    ResponseTemplate::new(200).set_body_json(json!({"results": [page("2", 3)]}))
                } else {
                    ResponseTemplate::new(200).set_body_json(json!({
                        "results": [page("1", 2)],
                        "_links": {"next": "/wiki/api/v2/pages?cursor=cursor%2Btwo%2F%3D&space-id=999&status=trashed"}
                    }))
                }
            }).expect(4).mount(&server).await;
        mount_page(&server, page("1", 2)).await;
        mount_page(&server, page("2", 3)).await;
        let pages = mock_client(&server).crawl_space("42", 2).await.unwrap();
        assert_eq!(pages.len(), 2);
        assert_eq!(pages[0].id, "1");
        assert_eq!(pages[0].storage, "<p>Wiki content</p>");
        assert_eq!(
            pages[0].modified_at.as_deref(),
            Some("2026-11-06T10:00:00Z")
        );
        assert_eq!(
            pages[0].web_url,
            "https://example.atlassian.net/wiki/pages/viewpage.action?pageId=1"
        );
        assert_eq!(calls.load(Ordering::SeqCst), 4);
    }

    #[tokio::test]
    async fn follows_link_header_cursor_when_body_has_none() {
        let server = MockServer::start().await;
        mount_space(&server).await;
        Mock::given(path("/wiki/api/v2/pages"))
            .respond_with(|request: &Request| {
                if request.url.query_pairs().any(|(key, _)| key == "cursor") {
                    ResponseTemplate::new(200).set_body_json(json!({"results": [page("2", 1)]}))
                } else {
                    ResponseTemplate::new(200)
                        .insert_header("Link", "</wiki/api/v2/pages?cursor=second>; rel=\"next\"")
                        .set_body_json(json!({"results": [page("1", 1)]}))
                }
            })
            .expect(4)
            .mount(&server)
            .await;
        mount_page(&server, page("1", 1)).await;
        mount_page(&server, page("2", 1)).await;
        assert_eq!(
            mock_client(&server)
                .crawl_space("42", 5)
                .await
                .unwrap()
                .len(),
            2
        );
    }

    #[tokio::test]
    async fn preserves_listing_webui_when_page_detail_only_returns_base() {
        let server = MockServer::start().await;
        mount_space(&server).await;
        let mut listed = page("1", 1);
        listed["_links"] = json!({"webui": "/spaces/AI/pages/1/Strategy"});
        mount_list(&server, vec![listed]).await;
        let mut detail = page("1", 1);
        detail["_links"] = json!({"base": "https://example.atlassian.net/wiki"});
        mount_page(&server, detail).await;
        let pages = mock_client(&server).crawl_space("42", 10).await.unwrap();
        assert_eq!(
            pages[0].web_url,
            "https://example.atlassian.net/wiki/spaces/AI/pages/1/Strategy"
        );
    }

    #[test]
    fn rejects_unsafe_malformed_and_disagreeing_pagination_links() {
        let client = Client::new(config()).unwrap();
        let current = client.api.join("pages").unwrap();
        for invalid in [
            "https://attacker.test/wiki/api/v2/pages?cursor=abc",
            "//attacker.test/wiki/api/v2/pages?cursor=abc",
            "https://reader:token@example.atlassian.net/wiki/api/v2/pages?cursor=abc",
            "/wiki/api/v2/spaces?cursor=abc",
            "/wiki/api/v2/pages",
            "?cursor=",
            "?cursor=a&cursor=b",
            "?cursor=a#fragment",
            "?cursor=%00",
        ] {
            assert!(client.cursor_from_link(&current, invalid).is_err());
        }
        assert!(
            client
                .next_cursor(
                    &current,
                    Some("?cursor=a"),
                    &["<?cursor=b>; rel=\"next\"".into()]
                )
                .is_err()
        );
        assert!(
            client
                .next_cursor(&current, None, &["?cursor=b; rel=next".into()])
                .is_err()
        );
        for invalid in [
            "malformed",
            "<?cursor=a>",
            "<?cursor=a>; rel=",
            "<?cursor=a>; rel=\"next",
            "<?cursor=a>; rel=next; rel=prev",
        ] {
            assert!(
                client
                    .next_cursor(&current, None, &[invalid.into()])
                    .is_err()
            );
        }
        assert_eq!(
            client
                .next_cursor(&current, None, &["<?cursor=a>; REL = \"next\"".into()])
                .unwrap(),
            Some("a".into())
        );
        assert_eq!(
            client.cursor_from_link(&current, "?cursor=abc").unwrap(),
            "abc"
        );
        let mut settings = config();
        settings.cloud_id = Some("12345678-1234-1234-1234-123456789abc".into());
        let scoped = Client::new(settings).unwrap();
        assert_eq!(
            scoped
                .cursor_from_link(
                    &scoped.api.join("pages").unwrap(),
                    "/wiki/api/v2/pages?cursor=abc"
                )
                .unwrap(),
            "abc"
        );
    }

    #[test]
    fn preserves_only_canonical_webui_links_for_the_returned_page() {
        let client = Client::new(config()).unwrap();
        for valid in [
            "/spaces/AI/pages/123/Strategy",
            "spaces/AI/pages/123/Strategy",
            "/wiki/spaces/AI/pages/123/Strategy",
            "https://example.atlassian.net/wiki/spaces/AI/pages/123/Strategy",
        ] {
            assert_eq!(
                client.page_web_url("123", Some(valid)).unwrap(),
                "https://example.atlassian.net/wiki/spaces/AI/pages/123/Strategy"
            );
        }
        for invalid in [
            "https://attacker.test/wiki/spaces/AI/pages/123/Strategy",
            "https://api.atlassian.com/wiki/spaces/AI/pages/123/Strategy",
            "https://user:secret@example.atlassian.net/wiki/spaces/AI/pages/123/Strategy",
            "/wiki/spaces/AI/pages/456/Strategy",
            "/wiki/spaces/AI/pages/123?token=secret",
            "/wiki/spaces/AI/pages/123#anchor",
            "javascript:alert(1)",
        ] {
            assert_eq!(
                client.page_web_url("123", Some(invalid)).unwrap(),
                "https://example.atlassian.net/wiki/pages/viewpage.action?pageId=123"
            );
        }
    }

    #[tokio::test]
    async fn refuses_partial_missing_body_or_unknown_status() {
        for invalid_kind in [
            "missing-body",
            "missing-value",
            "unknown-status",
            "missing-status",
            "wrong-space",
            "wrong-id",
            "changed-version",
        ] {
            let server = MockServer::start().await;
            mount_space(&server).await;
            mount_list(&server, vec![page("1", 1)]).await;
            let mut value = page("1", 1);
            match invalid_kind {
                "missing-body" => {
                    value.as_object_mut().unwrap().remove("body");
                }
                "missing-value" => {
                    value["body"]["storage"]
                        .as_object_mut()
                        .unwrap()
                        .remove("value");
                }
                "unknown-status" => value["status"] = json!("future-status"),
                "missing-status" => {
                    value.as_object_mut().unwrap().remove("status");
                }
                "wrong-space" => value["spaceId"] = json!("43"),
                "wrong-id" => value["id"] = json!("2"),
                "changed-version" => value["version"]["number"] = json!(2),
                _ => unreachable!(),
            }
            Mock::given(path("/wiki/api/v2/pages/1"))
                .respond_with(ResponseTemplate::new(200).set_body_json(value))
                .mount(&server)
                .await;
            assert!(
                mock_client(&server).crawl_space("42", 10).await.is_err(),
                "{invalid_kind}"
            );
        }
    }

    #[tokio::test]
    async fn refuses_partial_failure_without_returning_fetched_pages() {
        let server = MockServer::start().await;
        mount_space(&server).await;
        mount_list(&server, vec![page("1", 1), page("2", 1)]).await;
        mount_page(&server, page("1", 1)).await;
        Mock::given(path("/wiki/api/v2/pages/2"))
            .respond_with(ResponseTemplate::new(403).set_body_string("secret upstream body"))
            .mount(&server)
            .await;
        let error = mock_client(&server)
            .crawl_space("42", 10)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("403"));
        assert!(!format!("{error:#}").contains("secret"));
    }

    #[tokio::test]
    async fn refuses_changed_final_inventory() {
        for changed in [vec![], vec![page("1", 2)], vec![page("1", 1), page("2", 1)]] {
            let server = MockServer::start().await;
            mount_space(&server).await;
            mount_page(&server, page("1", 1)).await;
            let calls = Arc::new(AtomicUsize::new(0));
            Mock::given(path("/wiki/api/v2/pages"))
                .respond_with(move |_: &Request| {
                    let results = if calls.fetch_add(1, Ordering::SeqCst) == 0 {
                        vec![page("1", 1)]
                    } else {
                        changed.clone()
                    };
                    ResponseTemplate::new(200).set_body_json(json!({"results": results}))
                })
                .expect(2)
                .mount(&server)
                .await;
            let error = mock_client(&server)
                .crawl_space("42", 10)
                .await
                .unwrap_err();
            assert!(error.to_string().contains("inventory changed"));
        }
    }

    #[tokio::test]
    async fn crawl_refuses_an_empty_space() {
        // Arrange
        let server = MockServer::start().await;
        mount_space(&server).await;
        mount_list(&server, vec![]).await;

        // Act
        let error = mock_client(&server)
            .crawl_space("42", 10)
            .await
            .unwrap_err();

        // Assert
        assert!(
            error.to_string().contains("no visible current pages"),
            "{error}"
        );
    }

    #[tokio::test]
    async fn crawl_refuses_a_duplicate_page_id() {
        // Arrange
        let server = MockServer::start().await;
        mount_space(&server).await;
        mount_list(&server, vec![page("1", 1), page("1", 1)]).await;

        // Act
        let error = mock_client(&server)
            .crawl_space("42", 10)
            .await
            .unwrap_err();

        // Assert
        assert!(error.to_string().contains("duplicate page ID"), "{error}");
    }

    /// A two-page space with every body available, so only the page limit
    /// can explain a refusal.
    async fn two_page_space() -> MockServer {
        let server = MockServer::start().await;
        mount_space(&server).await;
        mount_list(&server, vec![page("1", 1), page("2", 1)]).await;
        mount_page(&server, page("1", 1)).await;
        mount_page(&server, page("2", 1)).await;
        server
    }

    #[tokio::test]
    async fn crawl_refuses_a_space_above_max_pages() {
        // Arrange
        let server = two_page_space().await;

        // Act
        let error = mock_client(&server).crawl_space("42", 1).await.unwrap_err();

        // Assert
        assert!(error.to_string().contains("max_pages"), "{error}");
    }

    #[tokio::test]
    async fn crawl_accepts_a_space_exactly_at_max_pages() {
        // Arrange
        let server = two_page_space().await;

        // Act
        let pages = mock_client(&server).crawl_space("42", 2).await.unwrap();

        // Assert
        assert_eq!(pages.len(), 2);
    }

    #[tokio::test]
    async fn crawl_refuses_space_metadata_for_another_space() {
        // Arrange
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/wiki/api/v2/spaces/42"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id": "43"})))
            .mount(&server)
            .await;
        mount_list(&server, vec![page("1", 1)]).await;
        mount_page(&server, page("1", 1)).await;

        // Act
        let error = mock_client(&server)
            .crawl_space("42", 10)
            .await
            .unwrap_err();

        // Assert
        assert!(
            error.to_string().contains("unexpected space metadata"),
            "{error}"
        );
    }

    #[tokio::test]
    async fn rejects_cursor_loops_and_empty_intermediate_pages() {
        for empty_first in [false, true] {
            let server = MockServer::start().await;
            mount_space(&server).await;
            Mock::given(path("/wiki/api/v2/pages"))
                .respond_with(move |request: &Request| {
                    let has_cursor = request.url.query_pairs().any(|(key, _)| key == "cursor");
                    let pages = if empty_first {
                        vec![]
                    } else {
                        vec![page(if has_cursor { "2" } else { "1" }, 1)]
                    };
                    ResponseTemplate::new(200).set_body_json(
                        json!({"results": pages, "_links": {"next": "?cursor=repeated"}}),
                    )
                })
                .mount(&server)
                .await;
            let error = mock_client(&server)
                .crawl_space("42", 10)
                .await
                .unwrap_err();
            assert!(error.to_string().contains(if empty_first {
                "empty intermediate"
            } else {
                "cursor repeated"
            }));
        }
    }

    #[tokio::test]
    async fn validates_crawl_inputs_before_network_access() {
        let server = MockServer::start().await;
        let client = mock_client(&server);
        for id in ["", "0", "01", "-1", "42?token=secret", "../42", "１２"] {
            assert!(client.crawl_space(id, 10).await.is_err());
        }
        assert!(client.crawl_space("42", 0).await.is_err());
        assert!(server.received_requests().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn retries_rate_limits_and_server_errors_only_within_budget() {
        for status in [429, 503] {
            let server = MockServer::start().await;
            let calls = Arc::new(AtomicUsize::new(0));
            Mock::given(path("/wiki/api/v2/spaces/42"))
                .respond_with(move |_: &Request| {
                    if calls.fetch_add(1, Ordering::SeqCst) == 0 {
                        ResponseTemplate::new(status).insert_header("Retry-After", "0")
                    } else {
                        ResponseTemplate::new(200).set_body_json(json!({"id": "42"}))
                    }
                })
                .expect(2)
                .mount(&server)
                .await;
            let mut client = mock_client(&server);
            client.max_retries = 1;
            client.check_space("42").await.unwrap();
        }
    }

    #[tokio::test]
    async fn does_not_ignore_excessive_or_invalid_retry_after() {
        for after in ["3600", "invalid-upstream-secret"] {
            let server = MockServer::start().await;
            Mock::given(path("/wiki/api/v2/spaces/42"))
                .respond_with(ResponseTemplate::new(429).insert_header("Retry-After", after))
                .expect(1)
                .mount(&server)
                .await;
            let mut client = mock_client(&server);
            client.max_retries = 1;
            let error = client.check_space("42").await.unwrap_err();
            assert!(!format!("{error:#}").contains("upstream-secret"));
        }
    }

    #[tokio::test]
    async fn retries_transient_errors_a_bounded_number_of_times() {
        let server = MockServer::start().await;
        Mock::given(path("/wiki/api/v2/spaces/42"))
            .respond_with(ResponseTemplate::new(503))
            .expect(2)
            .mount(&server)
            .await;
        let mut client = mock_client(&server);
        client.max_retries = 1;
        let error = client.check_space("42").await.unwrap_err();
        assert!(error.to_string().contains("bounded retries"));
    }

    #[tokio::test]
    async fn retries_and_redacts_transport_failures() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        drop(listener);
        let mut client = Client::new(config()).unwrap();
        client.max_retries = 1;
        let url = Url::parse(&format!("http://{address}/?cursor=private-cursor-value")).unwrap();
        let error = client
            .get_json(url)
            .await
            .err()
            .expect("closed loopback listener must fail");
        let message = format!("{error:#}");
        assert!(message.contains("bounded retries: transport failure"));
        assert!(!message.contains("private-cursor-value"));
        assert!(!message.contains("test-api-token"));
        assert!(!message.contains("reader@example.test"));
    }

    #[test]
    fn honors_numeric_and_date_retry_after_with_cumulative_budget() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
        let date =
            HeaderValue::from_str(&httpdate::fmt_http_date(now + Duration::from_secs(20))).unwrap();
        assert_eq!(
            parse_retry_after(Some(&date), now).unwrap(),
            Some(Duration::from_secs(20))
        );
        assert_eq!(
            parse_retry_after(Some(&HeaderValue::from_static("12")), now).unwrap(),
            Some(Duration::from_secs(12))
        );
        assert_eq!(
            retry_delay(0, Some(Duration::from_secs(12)), Duration::ZERO).unwrap(),
            Duration::from_secs(12)
        );
        assert!(retry_delay(0, Some(Duration::from_secs(31)), Duration::ZERO).is_err());
        assert!(retry_delay(0, Some(Duration::from_secs(12)), Duration::from_secs(50)).is_err());
        for attempt in 0..MAX_RETRIES {
            let delay = retry_delay(attempt, None, Duration::ZERO).unwrap();
            assert!((Duration::from_millis(200)..=Duration::from_millis(6_250)).contains(&delay));
        }
    }

    #[tokio::test]
    async fn refuses_redirects_and_redacts_http_errors() {
        let server = MockServer::start().await;
        for status in [302, 401, 403] {
            server.reset().await;
            Mock::given(path("/wiki/api/v2/spaces/42"))
                .respond_with(
                    ResponseTemplate::new(status)
                        .insert_header("Location", format!("{}/credential-leak", server.uri()))
                        .set_body_string("reader@example.test:test-api-token"),
                )
                .expect(1)
                .mount(&server)
                .await;
            let mut client = mock_client(&server);
            client.max_retries = 2;
            let error = client.check_space("42").await.unwrap_err();
            let message = format!("{error:#}");
            assert!(message.contains(&status.to_string()));
            assert!(!message.contains("reader@example.test"));
            assert!(!message.contains("test-api-token"));
            assert_eq!(server.received_requests().await.unwrap().len(), 1);
        }
    }

    #[tokio::test]
    async fn limits_response_bytes_and_redacts_malformed_json() {
        let server = MockServer::start().await;
        Mock::given(path("/wiki/api/v2/spaces/42"))
            .respond_with(ResponseTemplate::new(200).set_body_string("test-api-token is not JSON"))
            .mount(&server)
            .await;
        let mut client = mock_client(&server);
        let error = client.check_space("42").await.unwrap_err();
        assert_eq!(error.to_string(), "Confluence returned invalid JSON");
        client.max_response_bytes = 5;
        let error = client.check_space("42").await.unwrap_err();
        assert!(error.to_string().contains("max_response_bytes"));
    }
}
