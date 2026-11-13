// SPDX-License-Identifier: AGPL-3.0-only
//! Verify Forge Invocation Tokens (FITs).
//!
//! Forge sends every remote request with a FIT as its bearer token: a JWT
//! signed RS256 by Atlassian, issued by `forge/invocation-token`, for an
//! audience of the app's ID. A request is accepted only when the token
//! verifies against Atlassian's published keys *and* names the app and the
//! Confluence site this deployment was configured for, and no other site -
//! the configured values are the trusted source, never the token's own
//! claims. Isolated Cloud tokens, whose keys live elsewhere, are refused
//! rather than half-supported.

use std::sync::RwLock;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, ensure};
use jsonwebtoken::jwk::{JwkSet, KeyAlgorithm};
use jsonwebtoken::{Algorithm, DecodingKey, Validation, decode, decode_header};
use reqwest::Url;
use serde::Deserialize;

use super::printable;
use crate::client::is_uuid;

/// The issuer every FIT carries.
pub const FIT_ISSUER: &str = "forge/invocation-token";
/// Atlassian's published FIT signing keys for the commercial cloud.
pub const FORGE_JWKS_URL: &str = "https://forge.cdn.prod.atlassian-dev.net/.well-known/jwks.json";
/// Clock skew tolerated on `exp`/`nbf`; a FIT lives for about 25 seconds.
const CLOCK_LEEWAY_SECONDS: u64 = 10;
/// The shortest gap between two reads of the key set, so a stream of tokens
/// naming an unknown key cannot turn into a stream of fetches.
const JWKS_REFETCH_GAP: Duration = Duration::from_mins(1);
/// Bound on one key-set read; Forge allows a remote 5 seconds per event.
const JWKS_TIMEOUT: Duration = Duration::from_secs(3);
/// The largest key set accepted; Atlassian's is a few kilobytes.
const MAX_JWKS_BYTES: usize = 64 * 1024;
/// How a Forge app ID begins; a UUID follows.
const APP_ID_PREFIX: &str = "ari:cloud:ecosystem::app/";

/// Why a token was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FitError {
    /// Not a JWT, or a header this verifier cannot use.
    Malformed(String),
    /// Signed with an algorithm other than RS256.
    UnsupportedAlgorithm(String),
    /// Signed with a key Atlassian does not publish.
    UnknownKey(String),
    /// The signature or a standard claim (issuer, audience, expiry) failed.
    Rejected(String),
    /// Issued for another Forge app.
    WrongApp,
    /// Issued for an installation on another Confluence site.
    WrongSite,
    /// Issued in an Isolated Cloud environment, which this companion does not serve.
    IsolatedCloud,
    /// Atlassian's keys could not be read; the delivery should be retried.
    KeysUnavailable(String),
}

impl FitError {
    /// Whether retrying the same delivery later could succeed.
    #[must_use]
    pub fn is_transient(&self) -> bool {
        matches!(self, Self::KeysUnavailable(_))
    }
}

/// Shown with control characters escaped: a reason can carry text from a
/// token that has not been verified, and each one is logged as one line.
impl std::fmt::Display for FitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Malformed(why) => write!(f, "malformed invocation token: {}", printable(why)),
            Self::UnsupportedAlgorithm(alg) => {
                write!(
                    f,
                    "invocation token signed with {}, not RS256",
                    printable(alg)
                )
            }
            Self::UnknownKey(kid) => {
                write!(
                    f,
                    "invocation token signed with unknown key {}",
                    printable(kid)
                )
            }
            Self::Rejected(why) => write!(f, "invocation token rejected: {}", printable(why)),
            Self::WrongApp => f.write_str("invocation token issued for another Forge app"),
            Self::WrongSite => f.write_str("invocation token issued for another Confluence site"),
            Self::IsolatedCloud => {
                f.write_str("invocation token from an Isolated Cloud environment")
            }
            Self::KeysUnavailable(why) => {
                write!(f, "Forge signing keys unavailable: {}", printable(why))
            }
        }
    }
}

impl std::error::Error for FitError {}

/// The deployment's trusted identity: the Forge app and the Confluence site
/// it was installed on, both from configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Trust {
    app_id: String,
    cloud_id: String,
}

impl Trust {
    /// Pin deliveries to one app (`ari:cloud:ecosystem::app/<uuid>`) and one
    /// site's cloud ID (a UUID, compared case-insensitively).
    ///
    /// # Errors
    /// Returns an error when either value is not in its documented form.
    pub fn new(app_id: &str, cloud_id: &str) -> Result<Self> {
        ensure!(
            app_id.strip_prefix(APP_ID_PREFIX).is_some_and(is_uuid),
            "the Forge app ID must be {APP_ID_PREFIX}<uuid>, as `forge register` writes it"
        );
        ensure!(is_uuid(cloud_id), "the Confluence cloud ID must be a UUID");
        Ok(Self {
            app_id: app_id.to_owned(),
            cloud_id: cloud_id.to_ascii_lowercase(),
        })
    }

    #[must_use]
    pub fn app_id(&self) -> &str {
        &self.app_id
    }
}

/// The installation a verified delivery came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Invocation {
    /// `ari:cloud:ecosystem::installation/<uuid>`; stable across upgrades.
    pub installation_id: String,
    /// Where this installation's Atlassian API calls are routed, for use
    /// with the app system token that came with the delivery.
    pub api_base_url: Url,
}

/// A source of Atlassian's FIT signing keys.
pub trait KeySource: Send + Sync {
    /// The current key set. An implementation that caches reads it again
    /// when `kid` is missing from what it holds, at most once per gap, and
    /// while the gap holds that read back answers
    /// [`FitError::KeysUnavailable`], so the delivery is retried rather than
    /// refused. A key set returned without `kid` refuses the token.
    fn keys_for(&self, kid: &str) -> impl Future<Output = Result<JwkSet, FitError>> + Send;
}

/// Atlassian's key set over HTTPS, cached and read again only when a token
/// names a key the cache lacks (a rotation) - never on a timer.
pub struct ForgeKeys {
    http: reqwest::Client,
    url: Url,
    refetch_gap: Duration,
    cache: RwLock<Option<CachedKeys>>,
}

struct CachedKeys {
    keys: JwkSet,
    read_at: Instant,
}

impl ForgeKeys {
    /// Read keys from `url`, which must be HTTPS.
    ///
    /// # Errors
    /// Returns an error for a URL that is not HTTPS or an HTTP client that
    /// cannot be built.
    pub fn new(url: &str) -> Result<Self> {
        let url = Url::parse(url).context("invalid Forge key set URL")?;
        ensure!(
            url.scheme() == "https",
            "the Forge key set URL must be HTTPS"
        );
        let http = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(JWKS_TIMEOUT)
            .timeout(JWKS_TIMEOUT)
            .user_agent(concat!("pgokf-confluence/", env!("CARGO_PKG_VERSION")))
            .build()
            .context("building the HTTP client for Forge keys")?;
        Ok(Self {
            http,
            url,
            refetch_gap: JWKS_REFETCH_GAP,
            cache: RwLock::new(None),
        })
    }

    async fn fetch(&self) -> Result<JwkSet, FitError> {
        let unavailable = FitError::KeysUnavailable;
        let mut response = self
            .http
            .get(self.url.clone())
            .send()
            .await
            .map_err(|error| unavailable(format!("reading the key set: {error}")))?;
        let status = response.status();
        if !status.is_success() {
            return Err(unavailable(format!("the key set answered HTTP {status}")));
        }
        let too_large =
            || unavailable(format!("the key set is larger than {MAX_JWKS_BYTES} bytes"));
        if response
            .content_length()
            .is_some_and(|length| length > MAX_JWKS_BYTES as u64)
        {
            return Err(too_large());
        }
        let mut body = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|error| unavailable(format!("reading the key set: {error}")))?
        {
            body.extend_from_slice(&chunk);
            if body.len() > MAX_JWKS_BYTES {
                return Err(too_large());
            }
        }
        serde_json::from_slice(&body)
            .map_err(|error| unavailable(format!("the key set is not valid: {error}")))
    }
}

impl KeySource for ForgeKeys {
    async fn keys_for(&self, kid: &str) -> Result<JwkSet, FitError> {
        // Taken and dropped here, before the await below.
        let cached = {
            let guard = self.cache.read().ok();
            guard.and_then(|held| {
                held.as_ref().map(|cached| {
                    let known = cached.keys.find(kid).is_some();
                    let fresh = cached.read_at.elapsed() < self.refetch_gap;
                    (cached.keys.clone(), known, fresh)
                })
            })
        };
        match cached {
            Some((keys, true, _)) => return Ok(keys),
            // The key may have been published since the last read, which the
            // gap holds back: the delivery is retried once a read is allowed,
            // rather than refused for good.
            Some((_, false, true)) => {
                return Err(FitError::KeysUnavailable(format!(
                    "key {kid} is not among the keys read less than {} seconds ago",
                    self.refetch_gap.as_secs()
                )));
            }
            _ => {}
        }
        let keys = self.fetch().await?;
        if let Ok(mut held) = self.cache.write() {
            *held = Some(CachedKeys {
                keys: keys.clone(),
                read_at: Instant::now(),
            });
        }
        Ok(keys)
    }
}

/// Verifies FITs against a key source and the deployment's [`Trust`].
pub struct FitVerifier<K> {
    trust: Trust,
    keys: K,
}

impl<K: KeySource> FitVerifier<K> {
    #[must_use]
    pub fn new(trust: Trust, keys: K) -> Self {
        Self { trust, keys }
    }

    /// Verify one bearer token.
    ///
    /// # Errors
    /// Returns a [`FitError`] naming why the token was refused.
    pub async fn verify(&self, token: &str) -> Result<Invocation, FitError> {
        let header =
            decode_header(token).map_err(|error| FitError::Malformed(error.to_string()))?;
        // Only RS256, whatever the header asks for: the header is the
        // sender's to choose, and Atlassian signs FITs with RS256 alone.
        if header.alg != Algorithm::RS256 {
            return Err(FitError::UnsupportedAlgorithm(format!("{:?}", header.alg)));
        }
        let kid = header
            .kid
            .ok_or_else(|| FitError::Malformed("the token names no signing key".into()))?;
        let keys = self.keys.keys_for(&kid).await?;
        let jwk = keys
            .find(&kid)
            .ok_or_else(|| FitError::UnknownKey(kid.clone()))?;
        if jwk
            .common
            .key_algorithm
            .is_some_and(|declared| declared != KeyAlgorithm::RS256)
        {
            return Err(FitError::Rejected(format!("key {kid} is not an RS256 key")));
        }
        let key = DecodingKey::from_jwk(jwk)
            .map_err(|error| FitError::Rejected(format!("key {kid} is not usable: {error}")))?;
        let mut validation = Validation::new(Algorithm::RS256);
        validation.set_issuer(&[FIT_ISSUER]);
        validation.set_audience(&[&self.trust.app_id]);
        validation.required_spec_claims = ["exp", "iss", "aud"]
            .map(str::to_owned)
            .into_iter()
            .collect();
        validation.validate_exp = true;
        validation.validate_nbf = true;
        validation.leeway = CLOCK_LEEWAY_SECONDS;
        let claims = decode::<Claims>(token, &key, &validation)
            .map_err(|error| FitError::Rejected(error.to_string()))?
            .claims;
        if claims.ic_label.is_some() {
            return Err(FitError::IsolatedCloud);
        }
        // The configured app ID is the trusted source, not the token's own
        // claim: `aud` already matched it, and `app.id` must as well.
        if claims.app.id != self.trust.app_id {
            return Err(FitError::WrongApp);
        }
        // Every site the token names - in the invocation context and in the
        // installation's contexts - must be the configured one, and it must
        // name one: an installation elsewhere is refused even when the
        // invocation context names this site, and the other way round.
        let mut sites = claims
            .context
            .iter()
            .chain(
                claims
                    .app
                    .installation
                    .iter()
                    .flat_map(|i| i.contexts.iter()),
            )
            .filter_map(|context| context.cloud_id.as_deref())
            .peekable();
        let only_this_site = sites.peek().is_some()
            && sites.all(|cloud_id| cloud_id.eq_ignore_ascii_case(&self.trust.cloud_id));
        if !only_this_site {
            return Err(FitError::WrongSite);
        }
        let api_base_url = Url::parse(&claims.app.api_base_url)
            .ok()
            .filter(|url| url.scheme() == "https")
            .ok_or_else(|| FitError::Rejected("the app's API base URL is not HTTPS".into()))?;
        Ok(Invocation {
            installation_id: claims.app.installation_id,
            api_base_url,
        })
    }
}

#[derive(Deserialize)]
struct Claims {
    app: AppClaims,
    context: Option<SiteContext>,
    #[serde(rename = "icLabel")]
    ic_label: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct AppClaims {
    id: String,
    installation_id: String,
    api_base_url: String,
    installation: Option<Installation>,
}

#[derive(Deserialize)]
struct Installation {
    #[serde(default)]
    contexts: Vec<SiteContext>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SiteContext {
    cloud_id: Option<String>,
}

#[cfg(test)]
pub(crate) mod testing {
    //! Sign FITs with a throwaway RSA key, for this crate's tests.
    use aws_lc_rs::rand::SystemRandom;
    use aws_lc_rs::rsa::{KeyPair, KeySize, PublicKeyComponents};
    use aws_lc_rs::signature::{KeyPair as _, RSA_PKCS1_SHA256};
    use base64::Engine;
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use jsonwebtoken::jwk::JwkSet;
    use serde_json::{Value, json};

    use super::{FitError, KeySource};

    pub const APP_ID: &str = "ari:cloud:ecosystem::app/44444444-4444-4444-8444-444444444444";
    pub const CLOUD_ID: &str = "33333333-3333-4333-8333-333333333333";
    pub const KID: &str = "forge/invocation-token/wf-test-key";

    /// A throwaway signing key and the key set that publishes it.
    pub struct Signer {
        key: KeyPair,
        pub kid: String,
    }

    impl Signer {
        pub fn new(kid: &str) -> Self {
            Self {
                key: KeyPair::generate(KeySize::Rsa2048).expect("RSA key generation"),
                kid: kid.to_owned(),
            }
        }

        pub fn jwks(&self) -> JwkSet {
            let public = PublicKeyComponents::<Vec<u8>>::from(self.key.public_key());
            serde_json::from_value(json!({"keys": [{
                "kid": self.kid, "alg": "RS256", "use": "sig", "kty": "RSA",
                "n": URL_SAFE_NO_PAD.encode(&public.n),
                "e": URL_SAFE_NO_PAD.encode(&public.e),
            }]}))
            .expect("a well-formed key set")
        }

        pub fn sign(&self, header: &Value, claims: &Value) -> String {
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

        /// A valid FIT for [`APP_ID`] on [`CLOUD_ID`], issued now.
        pub fn token(&self) -> String {
            self.sign(&self.header(), &claims())
        }

        pub fn header(&self) -> Value {
            json!({"alg": "RS256", "typ": "JWT", "kid": self.kid})
        }
    }

    pub fn now() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs()
    }

    /// The claims of a FIT delivered with an event, shaped as documented.
    pub fn claims() -> Value {
        let now = now();
        json!({
            "app": {
                "installationId": "ari:cloud:ecosystem::installation/55555555-5555-4555-8555-555555555555",
                "apiBaseUrl": format!("https://api.atlassian.com/ex/confluence/{CLOUD_ID}"),
                "id": APP_ID,
                "version": "1",
                "environment": {"type": "DEVELOPMENT", "id": "e-1"},
                "module": {"type": "core:endpoint", "key": "pgokf-confluence-events"},
                "installation": {
                    "id": "ari:cloud:ecosystem::installation/55555555-5555-4555-8555-555555555555",
                    "contexts": [{
                        "name": "confluence",
                        "apiBaseUrl": format!("https://api.atlassian.com/ex/confluence/{CLOUD_ID}"),
                        "cloudId": CLOUD_ID
                    }]
                }
            },
            "aud": APP_ID,
            "iss": "forge/invocation-token",
            "iat": now,
            "nbf": now,
            "exp": now + 25,
            "jti": "d8a496253ec8c18a54631e4c82cbedd5d0ae8570"
        })
    }

    /// Keys served from memory.
    pub struct StaticKeys(pub JwkSet);

    impl KeySource for StaticKeys {
        async fn keys_for(&self, _kid: &str) -> Result<JwkSet, FitError> {
            Ok(self.0.clone())
        }
    }

    /// A key source that is always down.
    pub struct NoKeys;

    impl KeySource for NoKeys {
        async fn keys_for(&self, _kid: &str) -> Result<JwkSet, FitError> {
            Err(FitError::KeysUnavailable("connection refused".into()))
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::testing::{APP_ID, CLOUD_ID, KID, NoKeys, Signer, StaticKeys, claims, now};
    use super::*;

    fn trust() -> Trust {
        Trust::new(APP_ID, CLOUD_ID).unwrap()
    }

    fn verifier(signer: &Signer) -> FitVerifier<StaticKeys> {
        FitVerifier::new(trust(), StaticKeys(signer.jwks()))
    }

    #[tokio::test]
    async fn verify_accepts_a_token_signed_for_this_app_and_site() {
        // Arrange
        let signer = Signer::new(KID);
        let token = signer.token();

        // Act
        let invocation = verifier(&signer).verify(&token).await.unwrap();

        // Assert
        assert_eq!(
            invocation.installation_id,
            "ari:cloud:ecosystem::installation/55555555-5555-4555-8555-555555555555"
        );
        assert_eq!(
            invocation.api_base_url.as_str(),
            format!("https://api.atlassian.com/ex/confluence/{CLOUD_ID}")
        );
    }

    #[tokio::test]
    async fn verify_refuses_a_token_for_another_audience() {
        // Arrange
        let signer = Signer::new(KID);
        let mut claims = claims();
        claims["aud"] = json!("ari:cloud:ecosystem::app/11111111-1111-1111-1111-111111111111");
        let token = signer.sign(&signer.header(), &claims);

        // Act
        let error = verifier(&signer).verify(&token).await.unwrap_err();

        // Assert
        assert!(matches!(error, FitError::Rejected(_)), "{error}");
    }

    #[tokio::test]
    async fn verify_refuses_a_token_whose_app_claim_names_another_app() {
        // Arrange
        let signer = Signer::new(KID);
        let mut claims = claims();
        claims["app"]["id"] =
            json!("ari:cloud:ecosystem::app/11111111-1111-1111-1111-111111111111");
        let token = signer.sign(&signer.header(), &claims);

        // Act
        let error = verifier(&signer).verify(&token).await.unwrap_err();

        // Assert
        assert_eq!(error, FitError::WrongApp);
    }

    #[tokio::test]
    async fn verify_refuses_a_token_from_another_site() {
        // Arrange
        let signer = Signer::new(KID);
        let mut claims = claims();
        claims["app"]["installation"]["contexts"][0]["cloudId"] =
            json!("22222222-2222-2222-2222-222222222222");
        let token = signer.sign(&signer.header(), &claims);

        // Act
        let error = verifier(&signer).verify(&token).await.unwrap_err();

        // Assert
        assert_eq!(error, FitError::WrongSite);
    }

    #[tokio::test]
    async fn verify_refuses_a_token_that_names_no_site() {
        // Arrange
        let signer = Signer::new(KID);
        let mut claims = claims();
        claims["app"]
            .as_object_mut()
            .unwrap()
            .remove("installation");
        let token = signer.sign(&signer.header(), &claims);

        // Act
        let error = verifier(&signer).verify(&token).await.unwrap_err();

        // Assert
        assert_eq!(error, FitError::WrongSite);
    }

    #[tokio::test]
    async fn verify_accepts_the_site_named_in_the_invocation_context() {
        // Arrange
        let signer = Signer::new(KID);
        let mut claims = claims();
        claims["app"]
            .as_object_mut()
            .unwrap()
            .remove("installation");
        claims["context"] = json!({"cloudId": CLOUD_ID.to_uppercase()});
        let token = signer.sign(&signer.header(), &claims);

        // Act
        let result = verifier(&signer).verify(&token).await;

        // Assert
        assert!(result.is_ok(), "{result:?}");
    }

    #[tokio::test]
    async fn verify_refuses_an_expired_token() {
        // Arrange
        let signer = Signer::new(KID);
        let mut claims = claims();
        let issued = now() - 600;
        claims["iat"] = json!(issued);
        claims["nbf"] = json!(issued);
        claims["exp"] = json!(issued + 25);
        let token = signer.sign(&signer.header(), &claims);

        // Act
        let error = verifier(&signer).verify(&token).await.unwrap_err();

        // Assert
        assert!(matches!(error, FitError::Rejected(_)), "{error}");
    }

    #[tokio::test]
    async fn verify_refuses_another_issuer() {
        // Arrange
        let signer = Signer::new(KID);
        let mut claims = claims();
        claims["iss"] = json!("someone-else");
        let token = signer.sign(&signer.header(), &claims);

        // Act
        let error = verifier(&signer).verify(&token).await.unwrap_err();

        // Assert
        assert!(matches!(error, FitError::Rejected(_)), "{error}");
    }

    #[tokio::test]
    async fn verify_refuses_an_algorithm_other_than_rs256() {
        // Arrange
        let signer = Signer::new(KID);
        let header = json!({"alg": "HS256", "typ": "JWT", "kid": KID});
        let token = signer.sign(&header, &claims());

        // Act
        let error = verifier(&signer).verify(&token).await.unwrap_err();

        // Assert
        assert_eq!(error, FitError::UnsupportedAlgorithm("HS256".into()));
    }

    #[tokio::test]
    async fn verify_refuses_a_key_the_key_set_lacks() {
        // Arrange
        let published = Signer::new(KID);
        let impostor = Signer::new("forge/invocation-token/wf-unknown");
        let token = impostor.token();

        // Act
        let error = verifier(&published).verify(&token).await.unwrap_err();

        // Assert
        assert_eq!(
            error,
            FitError::UnknownKey("forge/invocation-token/wf-unknown".into())
        );
    }

    #[tokio::test]
    async fn verify_refuses_a_token_signed_by_another_key_under_a_published_id() {
        // Arrange
        let published = Signer::new(KID);
        let impostor = Signer::new(KID);
        let token = impostor.token();

        // Act
        let error = verifier(&published).verify(&token).await.unwrap_err();

        // Assert
        assert!(matches!(error, FitError::Rejected(_)), "{error}");
    }

    #[tokio::test]
    async fn verify_refuses_an_isolated_cloud_token() {
        // Arrange
        let signer = Signer::new(KID);
        let mut claims = claims();
        claims["icLabel"] = json!("yellow");
        let token = signer.sign(&signer.header(), &claims);

        // Act
        let error = verifier(&signer).verify(&token).await.unwrap_err();

        // Assert
        assert_eq!(error, FitError::IsolatedCloud);
    }

    #[tokio::test]
    async fn verify_refuses_an_api_base_url_that_is_not_https() {
        // Arrange
        let signer = Signer::new(KID);
        let mut claims = claims();
        claims["app"]["apiBaseUrl"] = json!("http://api.atlassian.com/ex/confluence/x");
        let token = signer.sign(&signer.header(), &claims);

        // Act
        let error = verifier(&signer).verify(&token).await.unwrap_err();

        // Assert
        assert!(matches!(error, FitError::Rejected(_)), "{error}");
    }

    #[tokio::test]
    async fn verify_refuses_a_token_that_is_not_a_jwt() {
        // Arrange
        let signer = Signer::new(KID);

        // Act
        let error = verifier(&signer).verify("not-a-token").await.unwrap_err();

        // Assert
        assert!(matches!(error, FitError::Malformed(_)), "{error}");
    }

    #[tokio::test]
    async fn verify_reports_unreadable_keys_as_transient() {
        // Arrange
        let signer = Signer::new(KID);
        let verifier = FitVerifier::new(trust(), NoKeys);

        // Act
        let error = verifier.verify(&signer.token()).await.unwrap_err();

        // Assert
        assert!(error.is_transient(), "{error}");
    }

    #[test]
    fn trust_new_accepts_documented_forms_and_rejects_others() {
        // Arrange
        let good_app = APP_ID;
        let good_cloud = CLOUD_ID.to_uppercase();

        // Act
        let accepted = Trust::new(good_app, &good_cloud);
        let bad_app = Trust::new("44444444-4444-4444-8444-444444444444", CLOUD_ID);
        let bad_cloud = Trust::new(APP_ID, "not-a-uuid");

        // Assert
        assert!(accepted.is_ok());
        assert!(bad_app.is_err());
        assert!(bad_cloud.is_err());
    }

    fn served_keys(signer: &Signer) -> ResponseTemplate {
        ResponseTemplate::new(200).set_body_json(serde_json::to_value(signer.jwks()).unwrap())
    }

    fn served_key_set(signers: &[&Signer]) -> ResponseTemplate {
        let keys = signers
            .iter()
            .flat_map(|signer| signer.jwks().keys)
            .collect();
        ResponseTemplate::new(200).set_body_json(serde_json::to_value(JwkSet { keys }).unwrap())
    }

    fn keys_at(server: &MockServer, gap: Duration) -> ForgeKeys {
        let mut keys = ForgeKeys::new(FORGE_JWKS_URL).unwrap();
        // An insecure destination is deliberately available only to unit tests.
        keys.url = Url::parse(&format!("{}/jwks.json", server.uri())).unwrap();
        keys.refetch_gap = gap;
        keys
    }

    #[tokio::test]
    async fn forge_keys_reads_the_key_set_once_for_a_known_key() {
        // Arrange
        let server = MockServer::start().await;
        let signer = Signer::new(KID);
        Mock::given(method("GET"))
            .and(path("/jwks.json"))
            .respond_with(served_keys(&signer))
            .expect(1)
            .mount(&server)
            .await;
        let keys = keys_at(&server, Duration::ZERO);

        // Act
        keys.keys_for(KID).await.unwrap();
        let second = keys.keys_for(KID).await.unwrap();

        // Assert
        assert!(second.find(KID).is_some());
    }

    #[tokio::test]
    async fn forge_keys_reads_again_for_an_unknown_key_after_the_gap() {
        // Arrange
        let server = MockServer::start().await;
        let signer = Signer::new(KID);
        Mock::given(method("GET"))
            .and(path("/jwks.json"))
            .respond_with(served_keys(&signer))
            .expect(2)
            .mount(&server)
            .await;
        let keys = keys_at(&server, Duration::ZERO);

        // Act
        keys.keys_for(KID).await.unwrap();
        let result = keys
            .keys_for("forge/invocation-token/wf-rotated")
            .await
            .unwrap();

        // Assert
        assert!(result.find("forge/invocation-token/wf-rotated").is_none());
    }

    #[tokio::test]
    async fn forge_keys_does_not_read_again_within_the_gap() {
        // Arrange
        let server = MockServer::start().await;
        let signer = Signer::new(KID);
        Mock::given(method("GET"))
            .and(path("/jwks.json"))
            .respond_with(served_keys(&signer))
            .expect(1)
            .mount(&server)
            .await;
        let keys = keys_at(&server, Duration::from_hours(1));

        // Act
        keys.keys_for(KID).await.unwrap();
        let error = keys
            .keys_for("forge/invocation-token/wf-rotated")
            .await
            .unwrap_err();

        // Assert: not read again (the mock's `expect(1)` is verified when the
        // server drops), and the key may yet be published, so the delivery
        // is to be retried rather than refused for good.
        assert!(error.is_transient(), "{error}");
    }

    #[tokio::test]
    async fn forge_keys_learns_a_rotated_key_once_the_gap_has_passed() {
        // Arrange: Atlassian publishes the next key between the two reads.
        let server = MockServer::start().await;
        let current = Signer::new(KID);
        let rotated = Signer::new("forge/invocation-token/wf-rotated");
        Mock::given(method("GET"))
            .and(path("/jwks.json"))
            .respond_with(served_key_set(&[&current]))
            .up_to_n_times(1)
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/jwks.json"))
            .respond_with(served_key_set(&[&current, &rotated]))
            .expect(1)
            .mount(&server)
            .await;
        let verifier = FitVerifier::new(trust(), keys_at(&server, Duration::ZERO));

        // Act
        let by_current = verifier.verify(&current.token()).await;
        let by_rotated = verifier.verify(&rotated.token()).await;
        let by_current_again = verifier.verify(&current.token()).await;

        // Assert: one read each for the first token and for the key it
        // lacked; a known key is never read again.
        assert!(by_current.is_ok(), "{by_current:?}");
        assert!(by_rotated.is_ok(), "{by_rotated:?}");
        assert!(by_current_again.is_ok(), "{by_current_again:?}");
    }

    #[tokio::test]
    async fn forge_keys_defers_a_key_it_has_not_seen_while_the_gap_holds_back_a_read() {
        // Arrange
        let server = MockServer::start().await;
        let current = Signer::new(KID);
        let rotated = Signer::new("forge/invocation-token/wf-rotated");
        Mock::given(method("GET"))
            .and(path("/jwks.json"))
            .respond_with(served_key_set(&[&current]))
            .expect(1)
            .mount(&server)
            .await;
        let verifier = FitVerifier::new(trust(), keys_at(&server, Duration::from_hours(1)));
        verifier.verify(&current.token()).await.unwrap();

        // Act
        let error = verifier.verify(&rotated.token()).await.unwrap_err();

        // Assert: a key published since the last read gets its delivery
        // retried after the gap, when it can be read, not refused for good.
        assert!(error.is_transient(), "{error}");
    }

    #[tokio::test]
    async fn forge_keys_reports_a_failed_read_as_unavailable() {
        // Arrange
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(503))
            .mount(&server)
            .await;
        let keys = keys_at(&server, Duration::ZERO);

        // Act
        let error = keys.keys_for(KID).await.unwrap_err();

        // Assert
        assert!(error.is_transient(), "{error}");
    }

    #[tokio::test]
    async fn forge_keys_refuses_an_oversized_key_set() {
        // Arrange
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(
                ResponseTemplate::new(200).set_body_string("x".repeat(MAX_JWKS_BYTES + 1)),
            )
            .mount(&server)
            .await;
        let keys = keys_at(&server, Duration::ZERO);

        // Act
        let error = keys.keys_for(KID).await.unwrap_err();

        // Assert
        assert!(error.to_string().contains("larger"), "{error}");
    }

    #[test]
    fn forge_keys_new_requires_https() {
        // Arrange
        let plain = "http://forge.example.test/jwks.json";

        // Act
        let result = ForgeKeys::new(plain);

        // Assert
        assert!(result.is_err());
    }
}
