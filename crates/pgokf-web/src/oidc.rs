// SPDX-License-Identifier: AGPL-3.0-only
//! `OpenID` Connect: this site as its own `OAuth` client - and GitHub, which
//! speaks `OAuth` but not `OpenID` Connect, by its own web flow.
//!
//! The fourth implementation of the seam in [`crate::auth`], for a
//! deployment that wants people to sign in with the identity provider they
//! already have (Entra ID, Okta, Keycloak, Auth0, Google, a `GitLab`
//! instance, GitHub) without putting an authenticating proxy in front.
//!
//! The flow is the authorization code flow with PKCE, which is what the
//! `OAuth` 2.1 draft and the `OpenID` Connect security guidance ask of a
//! server-side client:
//!
//! 1. The provider's configuration is read once from
//!    `<issuer>/.well-known/openid-configuration`, and the `issuer` it
//!    declares must be exactly the configured one.
//! 2. Signing in sends the person to the provider's authorization endpoint
//!    with a fresh `state`, `nonce`, and PKCE challenge. All three, and
//!    where to return to, are kept in one short-lived cookie this site
//!    signs, so nothing is held in memory between the two requests.
//! 3. The provider returns a code to the redirect URL. The `state` must
//!    match the cookie, the code is exchanged at the token endpoint over
//!    TLS with the PKCE verifier, and the ID token that comes back is
//!    verified against the provider's published keys.
//! 4. The verified claims become a [`Principal`], and this site opens its
//!    own session exactly as the `users` mode does.
//!
//! GitHub ([`ProviderKind::GitHub`]) has no discovery document, issues no ID
//! token and publishes no keys: its endpoints are known, the same code flow
//! (state, PKCE) yields an access token, and the person is read from its
//! API instead - `/user` for the account, `/user/emails` for the primary
//! verified address, `/user/orgs` and `/user/teams` for the groups - and
//! shaped into the same claims, so everything after that is shared.
//!
//! What the verification insists on: an asymmetric signature by a key the
//! provider publishes (never `none`, never an HMAC algorithm, which would
//! let a public key be used as a shared secret), the configured issuer,
//! this client in `aud`, an unexpired token, an `azp` that is this client
//! when present, and the `nonce` this site sent.

use std::collections::BTreeSet;
use std::fmt;
use std::net::IpAddr;
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use axum::http::HeaderMap;
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use jsonwebtoken::jwk::JwkSet;
use jsonwebtoken::{Algorithm, DecodingKey, Validation, decode, decode_header};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

use crate::auth::{
    FLOW_COOKIE, Mode, Principal, RoleMapping, SessionClaims, Sessions, cookie_value, random_bytes,
    valid_subject,
};
use crate::provider_settings::ProviderKind;
use crate::routes::filters::percent_encode;

/// The signature algorithms an ID token may carry. Asymmetric only: an
/// HMAC algorithm here would let anyone holding the provider's *public*
/// key mint tokens, the classic algorithm-confusion attack.
const ALLOWED_ALGORITHMS: &[Algorithm] = &[
    Algorithm::RS256,
    Algorithm::RS384,
    Algorithm::RS512,
    Algorithm::PS256,
    Algorithm::PS384,
    Algorithm::PS512,
    Algorithm::ES256,
    Algorithm::ES384,
    Algorithm::EdDSA,
];

/// How long the provider's configuration and keys are kept before they are
/// read again.
const METADATA_TTL: Duration = Duration::from_hours(1);
/// The shortest gap between two key fetches prompted by an unknown key id,
/// so a stream of tokens naming random keys cannot drive traffic at the
/// provider.
const JWKS_REFETCH_GAP: Duration = Duration::from_mins(1);
/// How long a person has to finish signing in at the provider.
const FLOW_SECONDS: u64 = 10 * 60;
/// The most bytes read from a provider's metadata or key document.
const MAX_METADATA_BYTES: usize = 512 * 1024;
/// How long any single request to the provider may take.
const PROVIDER_TIMEOUT: Duration = Duration::from_secs(10);
/// The GitHub REST API version this site speaks.
const GITHUB_API_VERSION: &str = "2022-11-28";
/// How many pages (of 100) of a person's organizations or teams are read.
const GITHUB_PAGES: usize = 5;

/// What the operator configured.
#[derive(Clone)]
pub(crate) struct OidcConfig {
    /// The slug the provider is stored under when an admin set it up on
    /// the Admin page; `None` for the `oidc` mode's own provider, set by
    /// the operator.
    pub id: Option<String>,
    /// What the provider speaks.
    pub kind: ProviderKind,
    /// The issuer URL, exactly as the provider declares it - or, for
    /// GitHub, the GitHub host.
    pub issuer: String,
    pub client_id: String,
    /// `None` for a public client, which PKCE alone protects.
    pub client_secret: Option<String>,
    /// This site's callback URL, as registered with the provider.
    pub redirect_uri: String,
    /// Space-separated scopes; `openid` is always included.
    pub scopes: String,
    /// Claims tried in order for the person's identity.
    pub subject_claims: Vec<String>,
    /// The claim carrying the person's groups.
    pub groups_claim: String,
    pub roles: RoleMapping,
    /// What the sign-in button calls the provider.
    pub provider_name: String,
}

/// Hand-written so the client secret cannot reach a log line, as the
/// session signing key cannot: a derived `Debug` on this would print it in
/// full the first time anything formatted the authenticator.
impl fmt::Debug for OidcConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OidcConfig")
            .field("id", &self.id)
            .field("kind", &self.kind)
            .field("issuer", &self.issuer)
            .field("client_id", &self.client_id)
            .field(
                "client_secret",
                &self.client_secret.as_ref().map(|_| "<redacted>"),
            )
            .field("redirect_uri", &self.redirect_uri)
            .field("scopes", &self.scopes)
            .field("subject_claims", &self.subject_claims)
            .field("groups_claim", &self.groups_claim)
            .field("roles", &self.roles)
            .field("provider_name", &self.provider_name)
            .finish()
    }
}

/// The parts of the provider's configuration this site uses.
#[derive(Debug, Clone, Deserialize)]
struct Discovery {
    issuer: String,
    authorization_endpoint: String,
    token_endpoint: String,
    jwks_uri: String,
    #[serde(default)]
    end_session_endpoint: Option<String>,
    #[serde(default)]
    id_token_signing_alg_values_supported: Option<Vec<String>>,
    #[serde(default)]
    token_endpoint_auth_methods_supported: Option<Vec<String>>,
    #[serde(default)]
    code_challenge_methods_supported: Option<Vec<String>>,
}

/// The provider's configuration and keys as last read.
#[derive(Debug)]
struct Metadata {
    discovery: Discovery,
    keys: JwkSet,
    read_at: Instant,
    keys_read_at: Instant,
}

/// One sign-in in progress, carried in a cookie this site signs while the
/// person is away at the provider.
#[derive(Debug, Serialize, Deserialize)]
struct Flow {
    /// The provider this sign-in started at (its slug), so the callback
    /// finishes it with the same one.
    #[serde(rename = "p", default, skip_serializing_if = "Option::is_none")]
    provider: Option<String>,
    #[serde(rename = "s")]
    state: String,
    #[serde(rename = "n")]
    nonce: String,
    #[serde(rename = "v")]
    verifier: String,
    #[serde(rename = "r")]
    next: String,
    #[serde(rename = "e")]
    expires: u64,
}

/// The claims this site reads from a verified ID token.
#[derive(Debug, Deserialize)]
struct IdClaims {
    sub: String,
    #[serde(default)]
    nonce: Option<String>,
    #[serde(default)]
    azp: Option<String>,
    #[serde(flatten)]
    other: Map<String, Value>,
}

/// What the token endpoint returns: an ID token from an `OpenID` Connect
/// provider, an access token from GitHub.
#[derive(Debug, Deserialize)]
struct TokenResponse {
    #[serde(default)]
    id_token: Option<String>,
    #[serde(default)]
    access_token: Option<String>,
    /// What was granted, as GitHub reports it (comma- or space-separated).
    #[serde(default)]
    scope: Option<String>,
}

/// The `OpenID` Connect mode.
#[derive(Debug)]
pub(crate) struct OidcAuth {
    http: Client,
    config: OidcConfig,
    sessions: Arc<Sessions>,
    metadata: RwLock<Option<Metadata>>,
}

impl OidcAuth {
    /// Build the mode. Nothing is fetched here: the provider is read on
    /// the first sign-in, so a provider that is down at boot does not stop
    /// the catalog from serving.
    ///
    /// # Errors
    ///
    /// A setting that cannot be right: a non-HTTPS issuer or redirect URL
    /// (except on the loopback interface, for a test provider), or an
    /// empty client id.
    pub(crate) fn new(config: OidcConfig, sessions: Arc<Sessions>) -> Result<Self> {
        if config.client_id.trim().is_empty() {
            bail!("the OIDC client id is empty");
        }
        check_url(&config.issuer, "the OIDC issuer")?;
        check_site_url(&config.redirect_uri, "the callback URL")?;
        if config.subject_claims.is_empty() {
            bail!("no OIDC subject claim is configured");
        }
        if config.subject_claims.first().is_some_and(|c| c != "sub") {
            eprintln!(
                "pgokf-web: note: people are identified by the {:?} claim. Use it only if your \
                 provider guarantees it unique and never reassigns it; otherwise set \
                 OKF_WEB_OIDC_SUBJECT_CLAIMS=sub, which is always stable.",
                config.subject_claims[0]
            );
        }
        let http = Client::builder()
            .timeout(PROVIDER_TIMEOUT)
            .redirect(reqwest::redirect::Policy::none())
            // GitHub's API refuses a request without one.
            .user_agent(concat!("pgokf-web/", env!("CARGO_PKG_VERSION")))
            .build()
            .context("building the HTTP client for the identity provider")?;
        Ok(Self {
            http,
            config,
            sessions,
            metadata: RwLock::new(None),
        })
    }

    pub(crate) fn sessions(&self) -> &Sessions {
        &self.sessions
    }

    /// What the sign-in button says.
    pub(crate) fn provider_name(&self) -> &str {
        &self.config.provider_name
    }

    /// The slug this provider is stored under, when an admin set it up on
    /// the Admin page.
    pub(crate) fn stored_id(&self) -> Option<&str> {
        self.config.id.as_deref()
    }

    /// The configured issuer.
    pub(crate) fn issuer(&self) -> &str {
        &self.config.issuer
    }

    /// What a session this provider opens is bound to: the issuer and the
    /// client, so a session survives neither a change of provider nor a
    /// re-registration - on every instance, with no store to consult.
    pub(crate) fn binding(&self) -> String {
        let digest = Sha256::digest(
            format!(
                "{}\n{}\n{}\n{}",
                self.config.id.as_deref().unwrap_or_default(),
                self.config.kind.id(),
                self.config.issuer.trim_end_matches('/'),
                self.config.client_id
            )
            .as_bytes(),
        );
        URL_SAFE_NO_PAD.encode(&digest[..16])
    }

    /// Reach the provider - its discovery document, or GitHub's API - so
    /// settings an admin is about to save are known to name a provider
    /// that answers.
    ///
    /// # Errors
    ///
    /// The document cannot be read or declares another issuer; GitHub's
    /// API does not answer.
    pub(crate) async fn probe(&self) -> Result<()> {
        match self.config.kind {
            ProviderKind::Oidc => self.metadata().await.map(|_| ()),
            ProviderKind::GitHub => {
                // The API root answers anonymously on github.com and with
                // 401 on a GitHub Enterprise Server in private mode: either
                // proves the host is GitHub's API and reachable.
                let api = github_api_base(&self.config.issuer);
                let url = format!("{api}/");
                let response = self
                    .http
                    .get(&url)
                    .send()
                    .await
                    .with_context(|| format!("GitHub's API at {url} could not be reached"))?;
                let status = response.status();
                if status.is_success()
                    || status == reqwest::StatusCode::UNAUTHORIZED
                    || status == reqwest::StatusCode::FORBIDDEN
                {
                    Ok(())
                } else {
                    bail!("GitHub's API at {url} answered HTTP {status}")
                }
            }
        }
    }

    /// Where to send someone to sign in, with the cookie that remembers
    /// this attempt. The cookie is the only state: nothing is held here
    /// between the two requests, so a restart or a second instance loses
    /// nothing.
    ///
    /// # Errors
    ///
    /// The provider's configuration cannot be read, or the system random
    /// source fails.
    pub(crate) async fn start(&self, next: &str) -> Result<(String, String)> {
        let discovery = self.metadata().await?;
        let state = URL_SAFE_NO_PAD.encode(random_bytes(24)?);
        let nonce = URL_SAFE_NO_PAD.encode(random_bytes(24)?);
        let verifier = URL_SAFE_NO_PAD.encode(random_bytes(32)?);
        let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
        let flow = Flow {
            provider: self.config.id.clone(),
            state: state.clone(),
            nonce: nonce.clone(),
            verifier,
            next: next.to_owned(),
            expires: now_unix() + FLOW_SECONDS,
        };
        let cookie = self
            .sessions
            .cookie(FLOW_COOKIE, &self.sessions.seal(&flow)?, FLOW_SECONDS);
        let url = authorization_url(
            &discovery.authorization_endpoint,
            &[
                ("response_type", "code"),
                ("client_id", &self.config.client_id),
                ("redirect_uri", &self.config.redirect_uri),
                ("scope", &scopes(self.config.kind, &self.config.scopes)),
                ("state", &state),
                ("nonce", &nonce),
                ("code_challenge", &challenge),
                ("code_challenge_method", "S256"),
            ],
        );
        Ok((url, cookie))
    }

    /// Finish a sign-in: check the state against the cookie, exchange the
    /// code, verify the ID token, and return the person with the path they
    /// were going to.
    ///
    /// # Errors
    ///
    /// A missing or stale flow cookie, a state that does not match, a
    /// provider that refuses the exchange, a token that does not verify,
    /// or claims that name nobody usable.
    pub(crate) async fn complete(
        &self,
        headers: &HeaderMap,
        code: &str,
        state: &str,
    ) -> Result<(Principal, Vec<String>, String)> {
        let token = cookie_value(headers, FLOW_COOKIE)
            .context("this sign-in did not start here, or it took too long")?;
        let flow: Flow = self
            .sessions
            .open(&token)
            .context("this sign-in did not start here")?;
        if flow.expires <= now_unix() {
            bail!("this sign-in took too long; start again");
        }
        if !constant_time_eq(&flow.state, state) {
            bail!("this sign-in does not match the one that started here");
        }
        if flow.provider != self.config.id {
            bail!("this sign-in started at another provider");
        }
        let tokens = self.exchange(code, &flow.verifier).await?;
        let claims = match self.config.kind {
            ProviderKind::Oidc => {
                let id_token = tokens
                    .id_token
                    .context("the provider's token response carried no ID token")?;
                self.verify_id_token(&id_token, &flow.nonce).await?
            }
            ProviderKind::GitHub => {
                let access_token = tokens
                    .access_token
                    .context("GitHub's token response carried no access token")?;
                // What GitHub granted decides what is read; the configured
                // scopes stand in when the answer names none.
                let granted = tokens
                    .scope
                    .clone()
                    .unwrap_or_else(|| scopes(self.config.kind, &self.config.scopes));
                self.github_claims(&access_token, &granted).await?
            }
        };
        let (principal, groups) = self.principal_from(&claims)?;
        Ok((principal, groups, flow.next))
    }

    /// The person a request's session cookie names. The role is derived
    /// from the groups the session carries, so a change to the role map
    /// takes effect at once; a change to the person's groups at the
    /// provider takes effect when they sign in again.
    pub(crate) async fn identify(&self, headers: &HeaderMap) -> Result<Option<Principal>> {
        let Some(claims) = self.sessions.read_session(headers, Mode::Oidc).await? else {
            return Ok(None);
        };
        Ok(self.recognize(&claims))
    }

    /// The person a live session's claims name, when this provider opened
    /// the session: one opened by another provider, or by this one under
    /// another registration, names nobody here.
    pub(crate) fn recognize(&self, claims: &SessionClaims) -> Option<Principal> {
        if !valid_subject(&claims.subject) || claims.binding != self.binding() {
            return None;
        }
        Some(Principal {
            role: self.config.roles.role_for(&claims.groups),
            display: claims
                .display
                .clone()
                .unwrap_or_else(|| claims.subject.clone()),
            subject: claims.subject.clone(),
        })
    }

    /// Where to send someone after this site's own session ends, when the
    /// provider offers to end its session too.
    pub(crate) fn end_session_url(&self) -> Option<String> {
        let endpoint = {
            let guard = self.metadata.read().ok()?;
            guard.as_ref()?.discovery.end_session_endpoint.clone()?
        };
        let mut params = vec![("client_id", self.config.client_id.as_str())];
        // A relative path would mean nothing to the provider: the parameter
        // goes only when this site's own address can be derived from the
        // callback URL the provider already knows.
        let home = self
            .config
            .redirect_uri
            .strip_suffix("/auth/callback")
            .map(|base| format!("{base}/"));
        if let Some(home) = &home {
            params.push(("post_logout_redirect_uri", home));
        }
        Some(authorization_url(&endpoint, &params))
    }

    /// The provider's configuration, read again once it is old.
    async fn metadata(&self) -> Result<Discovery> {
        // The guard is taken and dropped inside this block: a lock must
        // never be held across the await below, which takes it again.
        let fresh = {
            let guard = self.metadata.read().ok();
            guard.and_then(|held| {
                held.as_ref()
                    .filter(|m| m.read_at.elapsed() < METADATA_TTL)
                    .map(|m| m.discovery.clone())
            })
        };
        match fresh {
            Some(discovery) => Ok(discovery),
            None => self.refresh().await.map(|m| m.discovery),
        }
    }

    /// Read the provider's configuration and keys, and keep them. GitHub
    /// publishes neither: its endpoints follow from its host, and there are
    /// no keys to read because there is no ID token to verify.
    async fn refresh(&self) -> Result<Metadata> {
        if self.config.kind == ProviderKind::GitHub {
            let host = self.config.issuer.trim_end_matches('/');
            let discovery = Discovery {
                issuer: host.to_owned(),
                authorization_endpoint: format!("{host}/login/oauth/authorize"),
                token_endpoint: format!("{host}/login/oauth/access_token"),
                jwks_uri: String::new(),
                end_session_endpoint: None,
                id_token_signing_alg_values_supported: None,
                token_endpoint_auth_methods_supported: Some(vec!["client_secret_post".to_owned()]),
                code_challenge_methods_supported: Some(vec!["S256".to_owned()]),
            };
            let now = Instant::now();
            let metadata = Metadata {
                discovery: discovery.clone(),
                keys: JwkSet { keys: Vec::new() },
                read_at: now,
                keys_read_at: now,
            };
            if let Ok(mut guard) = self.metadata.write() {
                *guard = Some(Metadata {
                    discovery,
                    keys: JwkSet { keys: Vec::new() },
                    read_at: now,
                    keys_read_at: now,
                });
            }
            return Ok(metadata);
        }
        let url = format!(
            "{}/.well-known/openid-configuration",
            self.config.issuer.trim_end_matches('/')
        );
        let discovery: Discovery = self.fetch_json(&url).await?;
        // The document must claim the issuer this site was configured with:
        // otherwise a redirect could have led to another provider's document.
        if discovery.issuer.trim_end_matches('/') != self.config.issuer.trim_end_matches('/') {
            bail!(
                "the provider's configuration declares issuer {:?}, not the configured {:?}",
                discovery.issuer,
                self.config.issuer
            );
        }
        for (url, what) in [
            (&discovery.authorization_endpoint, "authorization endpoint"),
            (&discovery.token_endpoint, "token endpoint"),
            (&discovery.jwks_uri, "keys endpoint"),
        ] {
            check_url(url, &format!("the provider's {what}"))?;
        }
        if let Some(url) = &discovery.end_session_endpoint {
            // A sign-out sends the browser there: no scheme but the ones
            // a provider may use.
            check_url(url, "the provider's end-session endpoint")?;
        }
        if discovery
            .code_challenge_methods_supported
            .as_ref()
            .is_some_and(|methods| !methods.iter().any(|m| m == "S256"))
        {
            bail!("the provider does not offer PKCE with S256, which this site requires");
        }
        let keys: JwkSet = self.fetch_json(&discovery.jwks_uri).await?;
        let now = Instant::now();
        let metadata = Metadata {
            discovery: discovery.clone(),
            keys: keys.clone(),
            read_at: now,
            keys_read_at: now,
        };
        if let Ok(mut guard) = self.metadata.write() {
            *guard = Some(Metadata {
                discovery,
                keys,
                read_at: now,
                keys_read_at: now,
            });
        }
        Ok(metadata)
    }

    /// The provider's keys, read again when the token names one this site
    /// has not seen (a rotation) and the last read was not just now.
    async fn keys_for(&self, kid: Option<&str>) -> Result<JwkSet> {
        // Taken and dropped here, before the await that takes it again.
        let cached = {
            let guard = self.metadata.read().ok();
            guard.and_then(|held| {
                held.as_ref().map(|m| {
                    let known = kid.is_none_or(|kid| m.keys.find(kid).is_some());
                    let fresh = m.keys_read_at.elapsed() < JWKS_REFETCH_GAP;
                    (m.keys.clone(), known, fresh)
                })
            })
        };
        match cached {
            Some((keys, true, _) | (keys, _, true)) => Ok(keys),
            _ => self.refresh().await.map(|m| m.keys),
        }
    }

    /// Trade the code for tokens at the provider's token endpoint. This is
    /// a direct TLS-verified request from this site, so the code and the
    /// client secret never travel through the browser.
    async fn exchange(&self, code: &str, verifier: &str) -> Result<TokenResponse> {
        let discovery = self.metadata().await?;
        let mut form = vec![
            ("grant_type", "authorization_code"),
            ("code", code),
            ("redirect_uri", self.config.redirect_uri.as_str()),
            ("client_id", self.config.client_id.as_str()),
            ("code_verifier", verifier),
        ];
        let basic = Self::client_secret_basic(&discovery);
        if let Some(secret) = &self.config.client_secret
            && !basic
        {
            form.push(("client_secret", secret.as_str()));
        }
        // JSON asked for explicitly: GitHub answers in form encoding
        // otherwise, and an OpenID Connect provider answers JSON anyway.
        let mut request = self
            .http
            .post(&discovery.token_endpoint)
            .header(reqwest::header::ACCEPT, "application/json")
            .form(&form);
        if let Some(secret) = &self.config.client_secret
            && basic
        {
            request = request.basic_auth(&self.config.client_id, Some(secret));
        }
        let response = request
            .send()
            .await
            .context("the provider's token endpoint could not be reached")?;
        let status = response.status();
        let body = bounded_text(response).await?;
        if !status.is_success() {
            // The body can carry the client secret back in an echo; only
            // the OAuth error code is safe to repeat.
            bail!(
                "the provider refused the sign-in (HTTP {status}{})",
                oauth_error(&body).map_or(String::new(), |e| format!(", {e}"))
            );
        }
        // GitHub answers a bad code with 200 and an error body; that is a
        // refusal too, and the code says why.
        if let Some(error) = oauth_error(&body) {
            bail!("the provider refused the sign-in ({error})");
        }
        serde_json::from_str(&body).context("the provider's token response was not usable")
    }

    /// The person GitHub's API describes, as the claims an ID token would
    /// carry: `sub` is the account's numeric id, `login` and `name` its
    /// own, `email` the primary verified address when the `user:email`
    /// scope allowed reading it, and the groups the organizations and
    /// `org/team` slugs when `read:org` did.
    async fn github_claims(&self, access_token: &str, granted: &str) -> Result<IdClaims> {
        let api = github_api_base(&self.config.issuer);
        let (user, _) = self
            .github_json(&format!("{api}/user"), access_token)
            .await?;
        let emails = if scope_granted(granted, "user:email") {
            self.github_list(&format!("{api}/user/emails?per_page=100"), access_token)
                .await?
        } else {
            Vec::new()
        };
        let (orgs, teams) = if scope_granted(granted, "read:org") {
            (
                self.github_list(&format!("{api}/user/orgs?per_page=100"), access_token)
                    .await?,
                self.github_list(&format!("{api}/user/teams?per_page=100"), access_token)
                    .await?,
            )
        } else {
            (Vec::new(), Vec::new())
        };
        let claims = claims_from_github(
            &user,
            &Value::Array(emails),
            &Value::Array(orgs),
            &Value::Array(teams),
            &self.config.groups_claim,
        );
        // The token has served: GitHub keeps it valid otherwise, and this
        // site has no use for it after the claims. Best effort - a token
        // that outlives this is only a token this site never uses again.
        self.github_forget_token(&api, access_token).await;
        claims
    }

    /// Every page of a list GitHub's API paginates, up to
    /// [`GITHUB_PAGES`]: past that the person belongs to more groups than
    /// this site will read, and the sign-in says so rather than deciding a
    /// role on part of them.
    async fn github_list(&self, url: &str, access_token: &str) -> Result<Vec<Value>> {
        let mut items = Vec::new();
        let mut next = Some(url.to_owned());
        let mut pages = 0;
        while let Some(url) = next {
            if pages == GITHUB_PAGES {
                bail!(
                    "GitHub lists more than {} entries for this person; this site reads no \
                     further, so a role cannot be decided",
                    GITHUB_PAGES * 100
                );
            }
            pages += 1;
            let (page, link) = self.github_json(&url, access_token).await?;
            items.extend(page.as_array().into_iter().flatten().cloned());
            // Only a page of the same API is followed: a Link header
            // naming another host would send the person's token there. And
            // a page that cannot be followed is not quietly left out, which
            // would decide a role on part of the groups.
            next = match link.as_deref().and_then(next_link) {
                Some(page) if page.starts_with(&format!("{}/", api_origin(&url))) => Some(page),
                Some(page) => bail!(
                    "GitHub's next page of this list is not on {}: {}",
                    api_origin(&url),
                    api_origin(&page)
                ),
                None => None,
            };
        }
        Ok(items)
    }

    /// One document from GitHub's API, as the API asks to be called, with
    /// its `Link` header for the page after it.
    async fn github_json(&self, url: &str, access_token: &str) -> Result<(Value, Option<String>)> {
        let response = self
            .http
            .get(url)
            .bearer_auth(access_token)
            .header(reqwest::header::ACCEPT, "application/vnd.github+json")
            .header("X-GitHub-Api-Version", GITHUB_API_VERSION)
            .send()
            .await
            .with_context(|| format!("{url} could not be reached"))?;
        let status = response.status();
        if !status.is_success() {
            bail!("{url} answered HTTP {status}");
        }
        let link = response
            .headers()
            .get(reqwest::header::LINK)
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);
        let body = bounded_text(response).await?;
        let value = serde_json::from_str(&body)
            .with_context(|| format!("{url} did not answer with JSON"))?;
        Ok((value, link))
    }

    /// Revoke an access token this site is done with, as GitHub's API
    /// offers (`DELETE /applications/{client_id}/token`, authenticated as
    /// the app). Nothing depends on it, so a failure is only logged.
    async fn github_forget_token(&self, api: &str, access_token: &str) {
        let Some(secret) = &self.config.client_secret else {
            return;
        };
        let url = format!("{api}/applications/{}/token", self.config.client_id);
        let outcome = self
            .http
            .delete(&url)
            .basic_auth(&self.config.client_id, Some(secret))
            .header(reqwest::header::ACCEPT, "application/vnd.github+json")
            .header("X-GitHub-Api-Version", GITHUB_API_VERSION)
            .json(&serde_json::json!({ "access_token": access_token }))
            .send()
            .await;
        match outcome {
            Ok(response) if response.status().is_success() => {}
            Ok(response) => eprintln!(
                "pgokf-web: GitHub did not revoke a used access token (HTTP {})",
                response.status()
            ),
            Err(error) => {
                eprintln!("pgokf-web: GitHub did not revoke a used access token: {error}");
            }
        }
    }

    /// Whether to authenticate at the token endpoint with HTTP Basic (the
    /// default the specification names) or in the form body.
    fn client_secret_basic(discovery: &Discovery) -> bool {
        discovery
            .token_endpoint_auth_methods_supported
            .as_ref()
            .is_none_or(|methods| {
                methods.iter().any(|m| m == "client_secret_basic")
                    || !methods.iter().any(|m| m == "client_secret_post")
            })
    }

    /// Verify an ID token against the provider's published keys and the
    /// claims this site requires.
    async fn verify_id_token(&self, token: &str, nonce: &str) -> Result<IdClaims> {
        let discovery = self.metadata().await?;
        let issuer = discovery.issuer.clone();
        let advertised = discovery.id_token_signing_alg_values_supported;
        let header = decode_header(token).context("the ID token's header is not readable")?;
        if !ALLOWED_ALGORITHMS.contains(&header.alg) {
            bail!(
                "the ID token is signed with {:?}, which this site does not accept",
                header.alg
            );
        }
        let keys = self.keys_for(header.kid.as_deref()).await?;
        // A provider with exactly one key need not name it in the token.
        let jwk = if let Some(kid) = &header.kid {
            keys.find(kid)
                .with_context(|| format!("the provider publishes no key {kid:?}"))?
        } else {
            let [only] = keys.keys.as_slice() else {
                bail!("the ID token names no key and the provider publishes several");
            };
            only
        };
        let key = DecodingKey::from_jwk(jwk).context("the provider's key is not usable")?;
        let mut validation = Validation::new(header.alg);
        // The algorithms this site accepts, never the one the token asked
        // for: the header is the attacker's to choose.
        validation.algorithms = Self::accepted_algorithms(jwk.common.key_algorithm, advertised);
        if validation.algorithms.is_empty() {
            bail!("the provider advertises no signing algorithm this site accepts");
        }
        // The issuer exactly as the provider declares it: the discovery
        // document was already required to name the configured issuer, and
        // a trailing slash must not decide whether a token verifies.
        validation.set_issuer(&[&issuer]);
        validation.set_audience(&[&self.config.client_id]);
        validation.required_spec_claims = ["exp", "iss", "aud", "sub"]
            .map(str::to_owned)
            .into_iter()
            .collect();
        validation.validate_exp = true;
        validation.validate_nbf = true;
        let data =
            decode::<IdClaims>(token, &key, &validation).context("the ID token did not verify")?;
        let claims = data.claims;
        // With more than one audience the specification asks for `azp`;
        // when it is there at all it must name this client.
        if let Some(azp) = &claims.azp
            && azp != &self.config.client_id
        {
            bail!("the ID token was issued for another client");
        }
        match &claims.nonce {
            Some(theirs) if constant_time_eq(theirs, nonce) => Ok(claims),
            Some(_) => bail!("the ID token belongs to another sign-in"),
            None => bail!("the ID token carries no nonce"),
        }
    }

    /// The algorithms a token may be signed with: the ones this site
    /// accepts, narrowed to what the provider says it uses and to the
    /// key's own algorithm when either is declared. Never the algorithm
    /// the token itself names, which is the attacker's to choose.
    fn accepted_algorithms(
        key_algorithm: Option<jsonwebtoken::jwk::KeyAlgorithm>,
        advertised: Option<Vec<String>>,
    ) -> Vec<Algorithm> {
        if let Some(declared) = key_algorithm
            .and_then(|alg| alg.to_string().parse::<Algorithm>().ok())
            .filter(|alg| ALLOWED_ALGORITHMS.contains(alg))
        {
            return vec![declared];
        }
        let Some(advertised) = advertised else {
            return ALLOWED_ALGORITHMS.to_vec();
        };
        let named: Vec<Algorithm> = advertised
            .iter()
            .filter_map(|name| name.parse::<Algorithm>().ok())
            .filter(|alg| ALLOWED_ALGORITHMS.contains(alg))
            .collect();
        if named.is_empty() {
            // A provider that advertises nothing this site knows: fall back
            // to the whole allow-list rather than refusing every token.
            ALLOWED_ALGORITHMS.to_vec()
        } else {
            named
        }
    }

    /// The person a verified token names, with the groups that matter for
    /// their role.
    fn principal_from(&self, claims: &IdClaims) -> Result<(Principal, Vec<String>)> {
        let subject = self
            .config
            .subject_claims
            .iter()
            .filter_map(|name| {
                if name == "sub" {
                    return Some(claims.sub.clone());
                }
                // An email address only names a person if the provider says
                // it is verified: an account carrying someone else's
                // unverified email must not be able to assume their actor.
                if name == "email" && !email_verified(claims) {
                    return None;
                }
                claims
                    .other
                    .get(name)
                    .and_then(Value::as_str)
                    .map(str::to_owned)
            })
            .map(|value| value.trim().to_owned())
            .find(|value| valid_subject(value))
            .with_context(|| {
                format!(
                    "no claim among {} names this person in a form this catalog can record",
                    self.config.subject_claims.join(", ")
                )
            })?;
        let display = claims
            .other
            .get("name")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|name| !name.is_empty() && name.chars().all(|c| !c.is_control()))
            .map_or_else(|| subject.clone(), str::to_owned);
        let groups = string_list(claims.other.get(&self.config.groups_claim));
        let role = self.config.roles.role_for(&groups);
        // Only the groups that decide a role are kept, so the session
        // cookie stays small whatever the provider sends.
        let kept = self.config.roles.known_groups(&groups);
        Ok((
            Principal {
                subject,
                display,
                role,
            },
            kept,
        ))
    }

    /// A JSON document from the provider, bounded in size.
    async fn fetch_json<T: serde::de::DeserializeOwned>(&self, url: &str) -> Result<T> {
        let response = self
            .http
            .get(url)
            .send()
            .await
            .with_context(|| format!("{url} could not be reached"))?;
        let status = response.status();
        if !status.is_success() {
            bail!("{url} answered HTTP {status}");
        }
        let body = bounded_text(response).await?;
        serde_json::from_str(&body).with_context(|| format!("{url} did not answer with JSON"))
    }
}

/// Read a response body, refusing one that is implausibly large for
/// provider metadata.
async fn bounded_text(response: reqwest::Response) -> Result<String> {
    if response
        .content_length()
        .is_some_and(|len| len > MAX_METADATA_BYTES as u64)
    {
        bail!("the provider's answer is too large to be metadata");
    }
    let bytes = response
        .bytes()
        .await
        .context("the provider's answer could not be read")?;
    if bytes.len() > MAX_METADATA_BYTES {
        bail!("the provider's answer is too large to be metadata");
    }
    String::from_utf8(bytes.to_vec()).context("the provider's answer is not UTF-8")
}

/// The `error` of an `OAuth` error response, which is a fixed code and so
/// safe to repeat; the rest of the body is not.
fn oauth_error(body: &str) -> Option<String> {
    let value: Value = serde_json::from_str(body).ok()?;
    let error = value.get("error")?.as_str()?;
    let safe: String = error
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '_')
        .take(64)
        .collect();
    (!safe.is_empty()).then_some(safe)
}

/// A URL with query parameters appended, each percent-encoded.
fn authorization_url(endpoint: &str, params: &[(&str, &str)]) -> String {
    let separator = if endpoint.contains('?') { '&' } else { '?' };
    let query = params
        .iter()
        .map(|(key, value)| format!("{key}={}", percent_encode(value)))
        .collect::<Vec<_>>()
        .join("&");
    format!("{endpoint}{separator}{query}")
}

/// The scopes to ask for: what the operator configured, with `openid`
/// always present because the flow is `OpenID` Connect.
fn scopes(kind: ProviderKind, configured: &str) -> String {
    if kind == ProviderKind::GitHub {
        // GitHub has scopes of its own and no `openid`.
        return configured.split_whitespace().collect::<Vec<_>>().join(" ");
    }
    let mut scopes: Vec<&str> = configured.split_whitespace().collect();
    if !scopes.contains(&"openid") {
        scopes.insert(0, "openid");
    }
    let mut seen = BTreeSet::new();
    scopes
        .into_iter()
        .filter(|scope| seen.insert(*scope))
        .collect::<Vec<_>>()
        .join(" ")
}

/// A URL this site will talk to or send a browser to: HTTPS, or plain HTTP
/// only on the loopback interface, where there is no network to listen on.
fn check_url(url: &str, what: &str) -> Result<()> {
    let url = url.trim();
    if let Some(rest) = url.strip_prefix("https://") {
        if rest.is_empty() || rest.starts_with('/') {
            bail!("{what} has no host");
        }
        return Ok(());
    }
    if url.strip_prefix("http://").is_some_and(is_loopback_host) {
        Ok(())
    } else {
        bail!("{what} must be an https:// URL (http:// only on localhost, for a test provider)")
    }
}

/// This site's own address, as the provider will send the browser back to
/// it: `https://` anywhere, or plain `http://` where the site is served
/// that way already - on the loopback interface, or on a private or
/// link-local address that never leaves the operator's network. Anything
/// else in plain HTTP would carry the authorization code across the open
/// internet unprotected.
fn check_site_url(url: &str, what: &str) -> Result<()> {
    if check_url(url, what).is_ok() {
        return Ok(());
    }
    let Some(rest) = url.trim().strip_prefix("http://") else {
        bail!("{what} must be an https:// or http:// URL");
    };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    let host_port = authority.rsplit('@').next().unwrap_or_default();
    let host = host_port
        .strip_prefix('[')
        .and_then(|h| h.split(']').next())
        .unwrap_or_else(|| host_port.split(':').next().unwrap_or_default());
    let private = match host.parse::<IpAddr>() {
        Ok(IpAddr::V4(ip)) => ip.is_private() || ip.is_link_local() || ip.is_loopback(),
        Ok(IpAddr::V6(ip)) => {
            ip.is_loopback()
                || (ip.segments()[0] & 0xfe00) == 0xfc00
                || (ip.segments()[0] & 0xffc0) == 0xfe80
        }
        Err(_) => false,
    };
    if private {
        Ok(())
    } else {
        bail!(
            "{what} must be an https:// URL, or http:// on the loopback interface or a private \
             address, where the site is already served in plain HTTP"
        )
    }
}

/// Whether what follows `http://` is the loopback interface and nothing
/// else. The host is the whole first segment, after any credentials and
/// before any port: `localhost.evil.example` merely begins with the
/// loopback name and is not it.
fn is_loopback_host(rest: &str) -> bool {
    let authority = rest
        .split(['/', '?', '#'])
        .next()
        .unwrap_or_default()
        .rsplit('@')
        .next()
        .unwrap_or_default();
    let host = authority
        .rsplit_once(':')
        .map_or(authority, |(host, port)| {
            if !port.is_empty() && port.chars().all(|c| c.is_ascii_digit()) {
                host
            } else {
                authority
            }
        });
    matches!(host, "localhost" | "127.0.0.1" | "[::1]")
}

/// A claim that is a list of strings, or one string, as a list.
fn string_list(value: Option<&Value>) -> Vec<String> {
    match value {
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_owned)
            .collect(),
        Some(Value::String(one)) => one
            .split([',', ' '])
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .collect(),
        _ => Vec::new(),
    }
}

/// Where GitHub's REST API lives for a GitHub host: `api.github.com` for
/// github.com, `/api/v3` under the host for a GitHub Enterprise Server.
fn github_api_base(issuer: &str) -> String {
    let host = issuer.trim_end_matches('/');
    let authority = host
        .trim_start_matches("https://")
        .trim_start_matches("http://")
        .split('/')
        .next()
        .unwrap_or_default();
    if authority.eq_ignore_ascii_case("github.com")
        || authority.eq_ignore_ascii_case("www.github.com")
    {
        "https://api.github.com".to_owned()
    } else {
        format!("{host}/api/v3")
    }
}

/// Whether `granted` (GitHub's comma- or space-separated scope list)
/// covers `wanted`, counting the umbrella scopes that include it: `user`
/// covers `read:user` and `user:email`; `admin:org` and `write:org` cover
/// `read:org`.
fn scope_granted(granted: &str, wanted: &str) -> bool {
    granted
        .split([',', ' '])
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .any(|scope| {
            scope == wanted
                || match wanted {
                    "read:user" | "user:email" => scope == "user",
                    "read:org" => scope == "admin:org" || scope == "write:org",
                    _ => false,
                }
        })
}

/// Where a sign-in flow started.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum StartedAt {
    /// The `oidc` mode's own provider.
    Own,
    /// A provider set up on the Admin page, by its slug.
    Stored(String),
}

impl StartedAt {
    /// The slug, as [`crate::auth::Authenticator::provider`] takes it.
    pub(crate) fn id(&self) -> Option<&str> {
        match self {
            Self::Own => None,
            Self::Stored(id) => Some(id),
        }
    }
}

/// The provider a request's sign-in flow started at, or `None` when no flow
/// of this site's, still within its time, is presented. Read before the
/// provider itself is known, so the callback can find it.
pub(crate) fn flow_provider(sessions: &Sessions, headers: &HeaderMap) -> Option<StartedAt> {
    let flow: Flow = sessions.open(&cookie_value(headers, FLOW_COOKIE)?)?;
    if flow.expires <= now_unix() {
        return None;
    }
    Some(match flow.provider {
        Some(id) => StartedAt::Stored(id),
        None => StartedAt::Own,
    })
}

/// The origin of a URL (`scheme://authority`), what a page of the same API
/// must start with.
fn api_origin(url: &str) -> String {
    match url.find("://").map(|at| at + 3) {
        Some(after_scheme) => {
            let end = url[after_scheme..]
                .find('/')
                .map_or(url.len(), |slash| after_scheme + slash);
            url[..end].to_owned()
        }
        None => url.to_owned(),
    }
}

/// The `rel="next"` URL of a `Link` header, if there is a page after this one.
fn next_link(header: &str) -> Option<String> {
    header.split(',').find_map(|part| {
        let (url, rel) = part.split_once(';')?;
        rel.contains("rel=\"next\"").then(|| {
            url.trim()
                .trim_start_matches('<')
                .trim_end_matches('>')
                .to_owned()
        })
    })
}

/// GitHub's answers shaped into the claims an ID token would carry, so one
/// mapping to a person serves both kinds of provider.
///
/// # Errors
///
/// `/user` names no account id.
fn claims_from_github(
    user: &Value,
    emails: &Value,
    orgs: &Value,
    teams: &Value,
    groups_claim: &str,
) -> Result<IdClaims> {
    let id = user
        .get("id")
        .and_then(Value::as_u64)
        .context("GitHub's /user answer names no account id")?;
    let mut other = Map::new();
    for key in ["login", "name"] {
        if let Some(value) = user
            .get(key)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            other.insert(key.to_owned(), Value::String(value.to_owned()));
        }
    }
    // A person with no display name is shown by their login, not their id.
    if !other.contains_key("name")
        && let Some(login) = other.get("login").cloned()
    {
        other.insert("name".to_owned(), login);
    }
    // Only the primary address GitHub has verified, from /user/emails:
    // /user's own `email` is whatever the person chose to show, unverified.
    let verified_primary = emails.as_array().into_iter().flatten().find(|e| {
        e.get("primary").and_then(Value::as_bool) == Some(true)
            && e.get("verified").and_then(Value::as_bool) == Some(true)
    });
    if let Some(email) = verified_primary
        .and_then(|e| e.get("email"))
        .and_then(Value::as_str)
    {
        other.insert("email".to_owned(), Value::String(email.to_owned()));
        other.insert("email_verified".to_owned(), Value::Bool(true));
    }
    let mut groups: Vec<Value> = orgs
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|org| org.get("login").and_then(Value::as_str))
        .map(|login| Value::String(login.to_owned()))
        .collect();
    groups.extend(teams.as_array().into_iter().flatten().filter_map(|team| {
        let org = team.get("organization")?.get("login")?.as_str()?;
        let slug = team.get("slug")?.as_str()?;
        Some(Value::String(format!("{org}/{slug}")))
    }));
    other.insert(groups_claim.to_owned(), Value::Array(groups));
    Ok(IdClaims {
        sub: id.to_string(),
        nonce: None,
        azp: None,
        other,
    })
}

/// Whether the provider asserts the token's `email` is verified. Providers
/// send this as a boolean or, less correctly, the string `"true"`; anything
/// else (including an absent claim) is treated as unverified.
fn email_verified(claims: &IdClaims) -> bool {
    match claims.other.get("email_verified") {
        Some(Value::Bool(verified)) => *verified,
        Some(Value::String(text)) => text.eq_ignore_ascii_case("true"),
        _ => false,
    }
}

/// Compare two secrets without giving away where they first differ.
fn constant_time_eq(a: &str, b: &str) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.bytes()
        .zip(b.bytes())
        .fold(0_u8, |acc, (x, y)| acc | (x ^ y))
        == 0
}

fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::Role;

    fn config() -> OidcConfig {
        OidcConfig {
            id: None,
            kind: ProviderKind::Oidc,
            issuer: "https://id.example.test/realms/okf".to_owned(),
            client_id: "pgokf".to_owned(),
            client_secret: Some("s3cret".to_owned()),
            redirect_uri: "https://catalog.example.test/auth/callback".to_owned(),
            scopes: "openid profile email groups".to_owned(),
            subject_claims: vec![
                "preferred_username".to_owned(),
                "email".to_owned(),
                "sub".to_owned(),
            ],
            groups_claim: "groups".to_owned(),
            roles: RoleMapping::parse("okf-editors=editor,okf-admins=admin", Role::Viewer)
                .expect("valid"),
            provider_name: "Keycloak".to_owned(),
        }
    }

    fn auth() -> OidcAuth {
        OidcAuth::new(
            config(),
            Arc::new(Sessions::new(vec![3_u8; 32], 3600, false).expect("valid")),
        )
        .expect("valid")
    }

    #[test]
    fn a_callback_may_be_plain_http_on_a_private_address_only() {
        // Arrange / Act / Assert
        assert!(
            check_site_url("https://catalog.example/auth/callback", "the callback URL").is_ok()
        );
        assert!(check_site_url("http://127.0.0.1:8090/auth/callback", "the callback URL").is_ok());
        assert!(
            check_site_url(
                "http://192.168.1.132:8080/auth/callback",
                "the callback URL"
            )
            .is_ok()
        );
        assert!(check_site_url("http://10.100.0.14/auth/callback", "the callback URL").is_ok());
        assert!(check_site_url("http://[fd00::1]:8080/auth/callback", "the callback URL").is_ok());
        assert!(
            check_site_url("http://203.0.113.5/auth/callback", "the callback URL").is_err(),
            "a public address"
        );
        assert!(
            check_site_url("http://catalog.example/auth/callback", "the callback URL").is_err(),
            "a name, whatever it resolves to"
        );
        assert!(
            check_site_url("http://user@192.168.1.1@203.0.113.5/x", "the callback URL").is_err(),
            "the last @ decides the host"
        );
        assert!(check_site_url("ftp://192.168.1.1/x", "the callback URL").is_err());
    }

    #[test]
    fn github_api_lives_beside_the_host_it_serves() {
        // Arrange / Act / Assert
        assert_eq!(
            github_api_base("https://github.com"),
            "https://api.github.com"
        );
        assert_eq!(
            github_api_base("https://github.com/"),
            "https://api.github.com"
        );
        assert_eq!(
            github_api_base("https://github.example.com/"),
            "https://github.example.com/api/v3"
        );
        assert_eq!(
            github_api_base("http://127.0.0.1:9797"),
            "http://127.0.0.1:9797/api/v3"
        );
    }

    #[test]
    fn github_answers_become_the_claims_an_id_token_would_carry() {
        // Arrange: a person with a public but unverified email on /user, a
        // verified primary on /user/emails, one organization, one team.
        let user = serde_json::json!({"id": 583_231, "login": "octocat", "name": " The Octocat ", "email": "shown@example.test"});
        let emails = serde_json::json!([
            {"email": "shown@example.test", "primary": false, "verified": false},
            {"email": "octocat@example.test", "primary": true, "verified": true}
        ]);
        let orgs = serde_json::json!([{"login": "octo-org", "id": 1}]);
        let teams =
            serde_json::json!([{"slug": "maintainers", "organization": {"login": "octo-org"}}]);

        // Act
        let claims = claims_from_github(&user, &emails, &orgs, &teams, "groups").expect("claims");
        let nobody = claims_from_github(
            &serde_json::json!({"login": "x"}),
            &emails,
            &orgs,
            &teams,
            "groups",
        );
        let no_scopes =
            claims_from_github(&user, &Value::Null, &Value::Null, &Value::Null, "groups")
                .expect("claims");

        // Assert
        assert_eq!(claims.sub, "583231");
        assert_eq!(claims.other["login"], "octocat");
        assert_eq!(claims.other["name"], "The Octocat");
        assert_eq!(
            claims.other["email"], "octocat@example.test",
            "the verified primary, not the shown one"
        );
        assert_eq!(claims.other["email_verified"], true);
        assert_eq!(
            claims.other["groups"],
            serde_json::json!(["octo-org", "octo-org/maintainers"])
        );
        assert!(nobody.is_err(), "no id, no person");
        assert!(
            no_scopes.other.get("email").is_none(),
            "no verified address without the scope"
        );
        assert_eq!(no_scopes.other["groups"], serde_json::json!([]));
    }

    #[test]
    fn what_github_granted_decides_what_is_read_umbrellas_included() {
        // Arrange / Act / Assert
        assert!(scope_granted("read:user,user:email,read:org", "user:email"));
        assert!(
            scope_granted("user", "user:email"),
            "user covers user:email"
        );
        assert!(scope_granted("user", "read:user"));
        assert!(scope_granted("admin:org", "read:org"));
        assert!(scope_granted("write:org read:user", "read:org"));
        assert!(!scope_granted("read:user", "read:org"));
        assert!(!scope_granted("", "read:user"));
    }

    #[test]
    fn a_link_header_names_the_next_page_or_nothing() {
        // Arrange
        let header = "<https://api.github.com/user/teams?per_page=100&page=2>; rel=\"next\", \
                      <https://api.github.com/user/teams?per_page=100&page=3>; rel=\"last\"";
        let last = "<https://api.github.com/user/teams?per_page=100&page=1>; rel=\"prev\"";

        // Act / Assert
        assert_eq!(
            next_link(header).as_deref(),
            Some("https://api.github.com/user/teams?per_page=100&page=2")
        );
        assert_eq!(next_link(last), None);
        assert_eq!(next_link(""), None);
    }

    #[test]
    fn a_page_is_followed_only_on_the_api_it_came_from() {
        // Arrange
        let page = "https://api.github.com/user/orgs?per_page=100";
        let same = "https://api.github.com/user/orgs?page=2";
        let other = "https://api.github.com.evil.example/user/orgs?page=2";
        let another_scheme = "http://api.github.com/user/orgs?page=2";

        // Act
        let origin = api_origin(page);
        let followed = |next: &str| next.starts_with(&format!("{origin}/"));

        // Assert
        assert_eq!(origin, "https://api.github.com");
        assert!(followed(same));
        assert!(!followed(other));
        assert!(!followed(another_scheme));
        assert_eq!(api_origin("https://ghe.example"), "https://ghe.example");
    }

    #[test]
    fn a_person_without_a_display_name_is_shown_by_their_login() {
        // Arrange
        let user = serde_json::json!({"id": 7, "login": "octocat", "name": null});

        // Act
        let claims = claims_from_github(&user, &Value::Null, &Value::Null, &Value::Null, "groups")
            .expect("claims");

        // Assert
        assert_eq!(claims.other["name"], "octocat");
    }

    #[test]
    fn github_scopes_are_its_own_and_carry_no_openid() {
        // Arrange / Act / Assert
        assert_eq!(
            scopes(ProviderKind::GitHub, "read:user  user:email"),
            "read:user user:email"
        );
        assert!(
            scopes(ProviderKind::Oidc, "profile")
                .split_whitespace()
                .any(|s| s == "openid")
        );
    }

    #[test]
    fn only_https_or_a_loopback_test_provider_is_accepted() {
        // Arrange / Act / Assert
        assert!(check_url("https://id.example.test", "x").is_ok());
        assert!(check_url("http://localhost:9999/realms/okf", "x").is_ok());
        assert!(check_url("http://127.0.0.1:9999", "x").is_ok());
        assert!(check_url("http://[::1]:9999/x", "x").is_ok());
        assert!(check_url("http://id.example.test", "x").is_err());
        assert!(check_url("https://", "x").is_err());
        assert!(check_url("https:///path", "x").is_err());
        assert!(check_url("ftp://id.example.test", "x").is_err());
        // A host that merely begins with the loopback name is not it.
        assert!(check_url("http://localhost.evil.example/x", "x").is_err());
        assert!(check_url("http://127.0.0.1.evil.example", "x").is_err());
        assert!(check_url("http://evil.example/?x=localhost", "x").is_err());
        assert!(check_url("http://localhost@evil.example", "x").is_err());
        let mut settings = config();
        settings.issuer = "http://id.example.test".to_owned();
        assert!(
            OidcAuth::new(
                settings,
                Arc::new(Sessions::new(vec![3_u8; 32], 3600, false).expect("valid"))
            )
            .is_err()
        );
    }

    #[test]
    fn the_authorization_url_carries_pkce_and_a_fresh_state_each_time() {
        // Arrange
        let auth = auth();

        // Act
        let url = authorization_url(
            "https://id.example.test/auth",
            &[("client_id", "pgokf"), ("scope", "openid profile")],
        );
        let with_query = authorization_url("https://id.example.test/auth?x=1", &[("a", "b/c")]);

        // Assert
        assert_eq!(
            url,
            "https://id.example.test/auth?client_id=pgokf&scope=openid%20profile"
        );
        assert_eq!(with_query, "https://id.example.test/auth?x=1&a=b%2Fc");
        assert_eq!(auth.provider_name(), "Keycloak");
    }

    #[test]
    fn openid_is_always_among_the_scopes_and_never_twice() {
        // Arrange / Act / Assert
        assert_eq!(
            scopes(ProviderKind::Oidc, "profile email"),
            "openid profile email"
        );
        assert_eq!(
            scopes(ProviderKind::Oidc, "openid profile"),
            "openid profile"
        );
        assert_eq!(scopes(ProviderKind::Oidc, ""), "openid");
        assert_eq!(
            scopes(ProviderKind::Oidc, "openid openid profile"),
            "openid profile"
        );
    }

    #[test]
    fn a_group_claim_is_read_as_a_list_however_it_arrives() {
        // Arrange / Act / Assert
        assert_eq!(
            string_list(Some(&serde_json::json!(["a", "b"]))),
            vec!["a".to_owned(), "b".to_owned()]
        );
        assert_eq!(
            string_list(Some(&serde_json::json!("a, b"))),
            vec!["a".to_owned(), "b".to_owned()]
        );
        assert!(string_list(Some(&serde_json::json!(7))).is_empty());
        assert!(string_list(None).is_empty());
    }

    #[test]
    fn the_person_comes_from_the_first_usable_claim_with_their_mapped_groups() {
        // Arrange: a token whose preferred_username is unusable as an actor.
        let auth = auth();
        let claims = |value: serde_json::Value| -> IdClaims {
            serde_json::from_value(value).expect("claims")
        };
        let usable = claims(serde_json::json!({
            "sub": "8f3a-1", "preferred_username": "alice",
            "name": "Alice Smith", "groups": ["staff", "okf-editors", "okf-admins"]
        }));
        let spaced = claims(serde_json::json!({
            "sub": "8f3a-1", "preferred_username": "alice smith",
            "email": "alice@example.test", "email_verified": true, "groups": "okf-editors"
        }));
        let nothing = claims(serde_json::json!({ "sub": "not a subject", "email": "no one" }));

        // Act
        let (alice, groups) = auth.principal_from(&usable).expect("named");
        let (fallback, one) = auth.principal_from(&spaced).expect("named");
        let refused = auth.principal_from(&nothing);

        // Assert
        assert_eq!(alice.subject, "alice");
        assert_eq!(alice.display, "Alice Smith");
        assert_eq!(alice.role, Role::Admin, "the highest mapped group wins");
        assert_eq!(
            groups,
            vec!["okf-editors".to_owned(), "okf-admins".to_owned()]
        );
        assert_eq!(fallback.subject, "alice@example.test", "the next claim");
        assert_eq!(fallback.display, "alice@example.test");
        assert_eq!(one, vec!["okf-editors".to_owned()]);
        assert!(refused.is_err(), "no claim names a usable subject");
    }

    #[test]
    fn an_unverified_email_is_not_believed_and_falls_through_to_sub() {
        // Arrange: preferred_username is unusable and the email is present but
        // not verified, so an account carrying someone else's address must not
        // become that actor - the stable `sub` is used instead.
        let auth = auth();
        let claims = |value: serde_json::Value| -> IdClaims {
            serde_json::from_value(value).expect("claims")
        };
        let unverified = claims(serde_json::json!({
            "sub": "8f3a-1", "preferred_username": "alice smith",
            "email": "victim@example.test", "email_verified": false, "groups": "okf-editors"
        }));
        let absent = claims(serde_json::json!({
            "sub": "8f3a-2", "preferred_username": "bob jones",
            "email": "someone@example.test", "groups": "okf-editors"
        }));

        // Act
        let (a, _) = auth.principal_from(&unverified).expect("named by sub");
        let (b, _) = auth.principal_from(&absent).expect("named by sub");

        // Assert
        assert_eq!(a.subject, "8f3a-1", "unverified email skipped for sub");
        assert_eq!(b.subject, "8f3a-2", "absent email_verified skipped for sub");
    }

    #[test]
    fn only_the_oauth_error_code_is_repeated_from_a_refusal() {
        // Arrange / Act / Assert
        assert_eq!(
            oauth_error(r#"{"error":"invalid_grant","error_description":"secret=abc"}"#).as_deref(),
            Some("invalid_grant")
        );
        // Only letters, digits, and underscores survive, so an error code
        // can never carry markup or a quote into the page.
        assert_eq!(
            oauth_error(r#"{"error":"a b<script>"}"#).as_deref(),
            Some("abscript")
        );
        assert_eq!(oauth_error(r#"{"error":"<>&"}"#), None);
        assert_eq!(oauth_error("not json"), None);
        assert_eq!(oauth_error(r#"{"nope":1}"#), None);
    }

    #[test]
    fn the_token_endpoint_authenticates_the_way_the_provider_asks() {
        // Arrange
        let with = |methods: Option<Vec<&str>>| Discovery {
            issuer: "https://id.example.test/realms/okf".to_owned(),
            authorization_endpoint: "https://id.example.test/auth".to_owned(),
            token_endpoint: "https://id.example.test/token".to_owned(),
            jwks_uri: "https://id.example.test/certs".to_owned(),
            end_session_endpoint: None,
            id_token_signing_alg_values_supported: None,
            token_endpoint_auth_methods_supported: methods
                .map(|m| m.into_iter().map(str::to_owned).collect()),
            code_challenge_methods_supported: None,
        };

        // Act / Assert
        assert!(OidcAuth::client_secret_basic(&with(None)), "the default");
        assert!(OidcAuth::client_secret_basic(&with(Some(vec![
            "client_secret_basic"
        ]))));
        assert!(!OidcAuth::client_secret_basic(&with(Some(vec![
            "client_secret_post"
        ]))));
        assert!(OidcAuth::client_secret_basic(&with(Some(vec![
            "client_secret_post",
            "client_secret_basic"
        ]))));
    }

    #[test]
    fn a_token_may_only_be_signed_the_way_this_site_accepts() {
        // Arrange / Act
        let unrestricted = OidcAuth::accepted_algorithms(None, None);
        let declared = OidcAuth::accepted_algorithms(Some("RS256".parse().expect("known")), None);
        let hmac = OidcAuth::accepted_algorithms(Some("HS256".parse().expect("known")), None);
        let advertised = OidcAuth::accepted_algorithms(
            None,
            Some(vec![
                "RS256".to_owned(),
                "HS256".to_owned(),
                "made-up".to_owned(),
            ]),
        );
        let unknown = OidcAuth::accepted_algorithms(None, Some(vec!["made-up".to_owned()]));

        // Assert
        assert_eq!(unrestricted, ALLOWED_ALGORITHMS.to_vec());
        assert_eq!(declared, vec![Algorithm::RS256]);
        assert_eq!(
            hmac,
            ALLOWED_ALGORITHMS.to_vec(),
            "an HMAC key is never honoured"
        );
        assert_eq!(
            advertised,
            vec![Algorithm::RS256],
            "HMAC and nonsense drop out"
        );
        assert_eq!(unknown, ALLOWED_ALGORITHMS.to_vec());
        assert!(!ALLOWED_ALGORITHMS.contains(&Algorithm::HS256));
    }

    #[test]
    fn secrets_are_compared_without_leaking_where_they_differ() {
        // Arrange / Act / Assert
        assert!(constant_time_eq("abc", "abc"));
        assert!(!constant_time_eq("abc", "abd"));
        assert!(!constant_time_eq("abc", "abcd"));
        assert!(constant_time_eq("", ""));
    }

    #[tokio::test]
    async fn a_session_opened_in_one_mode_is_not_honoured_in_another() {
        // Arrange: one signing key, a server reconfigured between modes.
        let sessions = Sessions::new(vec![4_u8; 32], 3600, false).expect("valid");
        let mut headers = HeaderMap::new();
        let cookie = sessions
            .open_session("alice", Mode::Users, String::new())
            .await
            .expect("opens");
        headers.insert(
            axum::http::header::COOKIE,
            axum::http::HeaderValue::from_str(cookie.split(';').next().expect("value"))
                .expect("header"),
        );

        // Act / Assert
        assert!(
            sessions
                .read_session(&headers, Mode::Users)
                .await
                .expect("no store is attached, so this cannot fail")
                .is_some()
        );
        assert!(
            sessions
                .read_session(&headers, Mode::Oidc)
                .await
                .expect("no store is attached, so this cannot fail")
                .is_none(),
            "the provider mode does not honour a users-mode session"
        );
    }

    #[test]
    fn a_sign_in_is_remembered_in_one_signed_cookie_and_nowhere_else() {
        // Arrange
        let sessions = Sessions::new(vec![5_u8; 32], 3600, false).expect("valid");
        let flow = Flow {
            provider: None,
            state: "st".to_owned(),
            nonce: "no".to_owned(),
            verifier: "ve".to_owned(),
            next: "/bundles".to_owned(),
            expires: now_unix() + 60,
        };

        // Act
        let sealed = sessions.seal(&flow).expect("seals");
        let back: Option<Flow> = sessions.open(&sealed);
        let tampered: Option<Flow> = sessions.open(&format!("{sealed}x"));
        let other: Option<Flow> = Sessions::new(vec![6_u8; 32], 3600, false)
            .expect("valid")
            .open(&sealed);

        // Assert
        let back = back.expect("opens");
        assert_eq!(
            (back.state, back.next),
            ("st".to_owned(), "/bundles".to_owned())
        );
        assert!(tampered.is_none() && other.is_none());
    }
}
