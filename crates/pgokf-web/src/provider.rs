// SPDX-License-Identifier: AGPL-3.0-only
//! The identity provider the `users` mode offers beside its own sign-in,
//! as set up on the Admin page: built from the catalog's settings, kept
//! while they stand, rebuilt when they change.
//!
//! Settings are read on the paths where a provider is *used to sign in*
//! (the sign-in page, the callback, sign-out) - one row, rarely - and never
//! on the request path: a session the provider opened is recognized by the
//! provider already built, and the first such request after a start builds
//! it once. A change saved on any instance is noticed by every other
//! through the row's stamp on its next sign-in.

use std::sync::{Arc, PoisonError, RwLock};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};

use crate::auth::{Role, Sessions};
use crate::oidc::OidcAuth;
use crate::provider_settings::{ProviderKind, ProviderSettings, ProviderSettingsStore};
use crate::seal::Sealer;

/// What the Admin page's form says.
#[derive(Clone)]
pub(crate) struct ProviderDraft {
    pub enabled: bool,
    pub kind: ProviderKind,
    pub issuer: String,
    pub client_id: String,
    pub client_secret: SecretChange,
    pub redirect_url: String,
    pub scopes: String,
    pub subject_claims: String,
    pub groups_claim: String,
    pub provider_name: String,
    pub role_map: String,
    pub default_role: Role,
}

/// What to do with the stored client secret, which the form never shows.
#[derive(Clone, PartialEq, Eq)]
pub(crate) enum SecretChange {
    /// Leave the stored one as it is (the field was left blank).
    Keep,
    /// Seal and store this one.
    Set(String),
    /// Forget it: the client is public, PKCE alone protects it.
    Clear,
}

impl std::fmt::Debug for SecretChange {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Keep => "Keep",
            Self::Set(_) => "Set(<redacted>)",
            Self::Clear => "Clear",
        })
    }
}

/// How long a provider built from the catalog's settings is believed on
/// the request path before the row is read again: the bound on how long
/// another instance's change (a role-map demotion, say) can go unnoticed
/// here.
const BELIEVED_FOR: Duration = Duration::from_secs(30);

/// What the settings last seen came to, and their stamp.
struct Live {
    stamp: String,
    at: Instant,
    state: State,
}

/// A provider, or the reason the stored settings do not make one.
enum State {
    Provider(Arc<OidcAuth>),
    /// The row is there and enabled, but this instance cannot build it: a
    /// client secret sealed under another session secret, or none to open
    /// it with. Sign-in goes on without the provider, and the Admin page
    /// says why.
    Unbuildable(String),
}

/// The slot the `users` mode keeps its provider in.
pub(crate) struct ProviderSlot {
    store: ProviderSettingsStore,
    /// Present when the operator set a session secret: the only key a
    /// client secret can be sealed under.
    sealer: Option<Sealer>,
    sessions: Arc<Sessions>,
    live: RwLock<Option<Live>>,
}

impl std::fmt::Debug for ProviderSlot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProviderSlot")
            .field("store", &self.store)
            .field("can_seal", &self.sealer.is_some())
            .finish_non_exhaustive()
    }
}

impl ProviderSlot {
    pub(crate) fn new(
        store: ProviderSettingsStore,
        sealer: Option<Sealer>,
        sessions: Arc<Sessions>,
    ) -> Self {
        Self {
            store,
            sealer,
            sessions,
            live: RwLock::new(None),
        }
    }

    /// Whether a client secret could be stored here.
    pub(crate) fn can_seal(&self) -> bool {
        self.sealer.is_some()
    }

    /// The provider as the catalog has it now: built on first use, kept
    /// while the settings' stamp stands, rebuilt when it moves, and `None`
    /// while no enabled provider is set up - or while the settings cannot
    /// be built into one (see [`Self::trouble`]), which must not stop the
    /// password sign-in beside it.
    ///
    /// # Errors
    ///
    /// The catalog cannot be read.
    pub(crate) async fn current(&self) -> Result<Option<Arc<OidcAuth>>> {
        let Some(settings) = self.store.load().await?.filter(|s| s.enabled) else {
            self.forget();
            return Ok(None);
        };
        let known = self
            .live
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .as_ref()
            .filter(|live| live.stamp == settings.updated_at)
            .map(Live::answer);
        if let Some(answer) = known {
            self.touch();
            return Ok(answer);
        }
        let state = match self.build(&settings) {
            Ok(provider) => State::Provider(Arc::new(provider)),
            Err(error) => {
                // Said once per change of the settings, not per request.
                eprintln!(
                    "pgokf-web: the identity provider set up on the Admin page cannot be used \
                     here: {error:#}"
                );
                State::Unbuildable(format!("{error:#}"))
            }
        };
        Ok(self.remember(settings.updated_at, state))
    }

    /// The provider last built, for the request path: without asking the
    /// catalog while it was built or confirmed recently, and read again
    /// when it was not - so another instance's change is seen here within
    /// [`BELIEVED_FOR`].
    ///
    /// # Errors
    ///
    /// The catalog cannot be read.
    pub(crate) async fn recent(&self) -> Result<Option<Arc<OidcAuth>>> {
        let believed = self
            .live
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .as_ref()
            .filter(|live| live.at.elapsed() < BELIEVED_FOR)
            .map(Live::answer);
        match believed {
            Some(answer) => Ok(answer),
            None => self.current().await,
        }
    }

    /// Why the stored settings cannot be used by this instance, if that is
    /// so: shown on the Admin page, where an admin can enter the client
    /// secret again after a session-secret rotation.
    pub(crate) fn trouble(&self) -> Option<String> {
        match self
            .live
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .as_ref()
            .map(|live| &live.state)
        {
            Some(State::Unbuildable(why)) => Some(why.clone()),
            _ => None,
        }
    }

    /// The settings as stored, for the Admin page's form.
    ///
    /// # Errors
    ///
    /// The catalog cannot be read.
    pub(crate) async fn settings(&self) -> Result<Option<ProviderSettings>> {
        self.store.load().await
    }

    /// Turn the form into settings ready to store, and prove they build a
    /// provider - URLs, client id, claims, role map - without touching the
    /// network. The provider comes back so the caller can ask it to reach
    /// the issuer before anything is stored.
    ///
    /// # Errors
    ///
    /// A setting that cannot be right, or a client secret with no session
    /// secret to seal it under.
    pub(crate) fn prepare(
        &self,
        draft: &ProviderDraft,
        stored: Option<&ProviderSettings>,
        by: &str,
    ) -> Result<(ProviderSettings, OidcAuth)> {
        // The catalog's own constraints, checked here first so a slip is a
        // message on the form rather than a refused row.
        let issuer = match (draft.kind, draft.issuer.trim()) {
            // GitHub's own host, unless a GitHub Enterprise Server is named.
            (ProviderKind::GitHub, "") => "https://github.com".to_owned(),
            (_, issuer) => issuer.to_owned(),
        };
        plain("the issuer URL", &issuer, 1, 2048)?;
        plain("the client id", &draft.client_id, 1, 512)?;
        plain("the callback URL", &draft.redirect_url, 1, 2048)?;
        for (what, url) in [
            ("the issuer URL", &issuer),
            ("the callback URL", &draft.redirect_url),
        ] {
            if url.trim().chars().any(char::is_whitespace) {
                bail!("{what} holds a space");
            }
        }
        if draft.kind == ProviderKind::GitHub
            && draft
                .scopes
                .split_whitespace()
                .any(|scope| scope == "openid" || scope == "profile")
        {
            bail!(
                "openid and profile are not GitHub scopes: ask for read:user user:email read:org \
                 (read:org only if the role map names organizations or teams)"
            );
        }
        plain("the scopes", &draft.scopes, 0, 512)?;
        plain("the identity claims", &draft.subject_claims, 1, 512)?;
        plain("the groups claim", &draft.groups_claim, 1, 128)?;
        if draft.groups_claim.trim().chars().any(char::is_whitespace) {
            bail!("the groups claim is one claim name, without spaces");
        }
        plain("the provider name", &draft.provider_name, 1, 64)?;
        plain("the role map", &draft.role_map, 0, 4096)?;
        if let SecretChange::Set(secret) = &draft.client_secret {
            plain("the client secret", secret, 1, 4096)?;
        }
        let client_secret = match &draft.client_secret {
            SecretChange::Keep => stored.and_then(|s| s.client_secret.clone()),
            SecretChange::Clear => None,
            SecretChange::Set(secret) => Some(
                self.sealer
                    .as_ref()
                    .context(
                        "a client secret can only be kept once this site has a session secret of \
                         its own: set OKF_WEB_SESSION_SECRET (at least 32 characters) and start \
                         again, or register a public client and leave the secret empty",
                    )?
                    .seal(secret)?,
            ),
        };
        if draft.kind == ProviderKind::GitHub && client_secret.is_none() {
            bail!(
                "a GitHub OAuth App cannot complete a sign-in without its client secret: enter \
                 it{}",
                if self.sealer.is_some() {
                    ""
                } else {
                    " once this site has an OKF_WEB_SESSION_SECRET to keep it under"
                }
            );
        }
        let settings = ProviderSettings {
            enabled: draft.enabled,
            kind: draft.kind,
            issuer,
            client_id: draft.client_id.trim().to_owned(),
            client_secret,
            redirect_url: draft.redirect_url.trim().to_owned(),
            scopes: draft.scopes.trim().to_owned(),
            subject_claims: draft.subject_claims.trim().to_owned(),
            groups_claim: draft.groups_claim.trim().to_owned(),
            provider_name: draft.provider_name.trim().to_owned(),
            role_map: draft.role_map.trim().to_owned(),
            default_role: draft.default_role,
            updated_at: String::new(),
            updated_by: by.to_owned(),
        };
        let provider = self.build(&settings)?;
        Ok((settings, provider))
    }

    /// Store settings [`prepare`](Self::prepare) produced, and keep the
    /// provider built and proven from them - so the one that answered the
    /// probe serves the next sign-in, with its metadata already read. What
    /// was stored comes back with the catalog's stamp.
    ///
    /// # Errors
    ///
    /// The catalog refuses the row or cannot be written.
    pub(crate) async fn store(
        &self,
        settings: &ProviderSettings,
        provider: OidcAuth,
    ) -> Result<ProviderSettings> {
        let saved = self.store.save(settings).await?;
        if saved.enabled {
            self.remember(
                saved.updated_at.clone(),
                State::Provider(Arc::new(provider)),
            );
        } else {
            self.forget();
        }
        Ok(saved)
    }

    /// Forget the provider altogether: `false` when none was set up.
    ///
    /// # Errors
    ///
    /// The catalog cannot be written.
    pub(crate) async fn remove(&self) -> Result<bool> {
        let removed = self.store.remove().await?;
        self.forget();
        Ok(removed)
    }

    fn build(&self, settings: &ProviderSettings) -> Result<OidcAuth> {
        OidcAuth::new(
            settings.config(self.sealer.as_ref())?,
            Arc::clone(&self.sessions),
        )
    }

    /// Keep what a read of the settings came to. A read that straddled a
    /// save - it saw the row before, and gets here after the saver
    /// remembered the newer settings - must not put the older ones back:
    /// the stamps order (an instant, to the microsecond), so a newer entry
    /// believed within [`BELIEVED_FOR`] stands.
    fn remember(&self, stamp: String, state: State) -> Option<Arc<OidcAuth>> {
        let mut slot = self.live.write().unwrap_or_else(PoisonError::into_inner);
        if let Some(newer) = slot
            .as_ref()
            .filter(|live| live.stamp > stamp && live.at.elapsed() < BELIEVED_FOR)
        {
            return newer.answer();
        }
        let live = Live {
            stamp,
            at: Instant::now(),
            state,
        };
        let answer = live.answer();
        *slot = Some(live);
        answer
    }

    /// The settings were read again and stand: believed afresh.
    fn touch(&self) {
        if let Some(live) = self
            .live
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .as_mut()
        {
            live.at = Instant::now();
        }
    }

    fn forget(&self) {
        *self.live.write().unwrap_or_else(PoisonError::into_inner) = None;
    }
}

impl Live {
    fn answer(&self) -> Option<Arc<OidcAuth>> {
        match &self.state {
            State::Provider(provider) => Some(Arc::clone(provider)),
            State::Unbuildable(_) => None,
        }
    }
}

/// One printable field within its bounds - what the catalog's constraints
/// say, said on the form instead.
fn plain(what: &str, value: &str, min: usize, max: usize) -> Result<()> {
    let value = value.trim();
    let length = value.chars().count();
    if length < min {
        bail!("{what} is missing");
    }
    if length > max {
        bail!("{what} is longer than {max} characters");
    }
    if value.chars().any(char::is_control) {
        bail!("{what} holds a control character");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const SECRET: &str = "a session secret of at least thirty-two characters";

    fn sessions() -> Arc<Sessions> {
        Arc::new(Sessions::new(SECRET.as_bytes().to_vec(), 3_600, false).expect("sessions"))
    }

    fn slot(sealer: bool) -> ProviderSlot {
        ProviderSlot::new(
            ProviderSettingsStore::memory(),
            sealer.then(|| Sealer::from_secret(SECRET.as_bytes()).expect("sealer")),
            sessions(),
        )
    }

    fn draft(name: &str) -> ProviderDraft {
        ProviderDraft {
            enabled: true,
            kind: ProviderKind::Oidc,
            issuer: "https://id.example".to_owned(),
            client_id: "catalog".to_owned(),
            client_secret: SecretChange::Clear,
            redirect_url: "https://catalog.example/auth/callback".to_owned(),
            scopes: "openid profile email".to_owned(),
            subject_claims: "sub".to_owned(),
            groups_claim: "groups".to_owned(),
            provider_name: name.to_owned(),
            role_map: "okf-admins=admin".to_owned(),
            default_role: Role::Viewer,
        }
    }

    #[tokio::test]
    async fn a_provider_is_built_once_and_rebuilt_when_the_settings_change() {
        // Arrange
        let slot = slot(false);
        let (settings, settings_provider) = slot
            .prepare(&draft("Okta"), None, "root")
            .expect("prepares");
        slot.store(&settings, settings_provider)
            .await
            .expect("stores");

        // Act
        let first = slot.current().await.expect("reads").expect("a provider");
        let again = slot.current().await.expect("reads").expect("a provider");
        let (changed, changed_provider) = slot
            .prepare(&draft("Entra ID"), Some(&settings), "root")
            .expect("prepares");
        slot.store(&changed, changed_provider)
            .await
            .expect("stores");
        let rebuilt = slot.current().await.expect("reads").expect("a provider");

        // Assert
        assert!(Arc::ptr_eq(&first, &again), "kept while the stamp stands");
        assert_eq!(first.provider_name(), "Okta");
        assert!(!Arc::ptr_eq(&first, &rebuilt), "rebuilt when it moves");
        assert_eq!(rebuilt.provider_name(), "Entra ID");
        assert!(
            slot.recent()
                .await
                .expect("reads")
                .is_some_and(|p| Arc::ptr_eq(&p, &rebuilt))
        );
    }

    #[tokio::test]
    async fn a_disabled_or_absent_provider_is_not_offered() {
        // Arrange
        let slot = slot(false);
        let none = slot.current().await.expect("reads");
        let mut off = draft("Okta");
        off.enabled = false;
        let (settings, settings_provider) = slot.prepare(&off, None, "root").expect("prepares");
        slot.store(&settings, settings_provider)
            .await
            .expect("stores");

        // Act
        let disabled = slot.current().await.expect("reads");
        let removed = slot.remove().await.expect("removes");

        // Assert
        assert!(none.is_none());
        assert!(disabled.is_none());
        assert!(slot.recent().await.expect("reads").is_none());
        assert!(removed);
        assert!(
            !slot.remove().await.expect("answers"),
            "nothing left to remove"
        );
    }

    #[tokio::test]
    async fn a_client_secret_needs_a_session_secret_and_is_stored_sealed() {
        // Arrange
        let without = slot(false);
        let with = slot(true);
        let mut confidential = draft("Okta");
        confidential.client_secret = SecretChange::Set("s3cret".to_owned());

        // Act
        let refused = without.prepare(&confidential, None, "root");
        let public = without.prepare(&draft("Okta"), None, "root");
        let (sealed, provider) = with.prepare(&confidential, None, "root").expect("prepares");

        // Assert
        assert!(
            refused
                .expect_err("no session secret, no client secret")
                .to_string()
                .contains("OKF_WEB_SESSION_SECRET")
        );
        assert!(public.is_ok(), "a public client needs no sealer");
        let stored = sealed.client_secret.as_deref().expect("stored sealed");
        assert!(stored.starts_with("v1:") && !stored.contains("s3cret"));
        assert_eq!(provider.provider_name(), "Okta");
        assert_eq!(
            sealed
                .config(with.sealer.as_ref())
                .expect("opens")
                .client_secret
                .as_deref(),
            Some("s3cret")
        );
    }

    #[tokio::test]
    async fn keep_leaves_the_stored_secret_and_clear_drops_it() {
        // Arrange
        let slot = slot(true);
        let mut confidential = draft("Okta");
        confidential.client_secret = SecretChange::Set("s3cret".to_owned());
        let (stored, stored_provider) =
            slot.prepare(&confidential, None, "root").expect("prepares");
        let stored = slot.store(&stored, stored_provider).await.expect("stores");

        // Act
        let mut renamed = draft("Okta again");
        renamed.client_secret = SecretChange::Keep;
        let (kept, _) = slot
            .prepare(&renamed, Some(&stored), "root")
            .expect("prepares");
        let (cleared, _) = slot
            .prepare(&draft("Okta"), Some(&stored), "root")
            .expect("prepares");

        // Assert
        assert_eq!(kept.client_secret, stored.client_secret);
        assert_eq!(cleared.client_secret, None);
    }

    #[tokio::test]
    async fn a_setting_that_cannot_be_right_is_refused_before_anything_is_stored() {
        // Arrange
        let slot = slot(false);
        let mut plain_http = draft("Okta");
        plain_http.issuer = "http://id.example".to_owned();
        let mut bad_map = draft("Okta");
        bad_map.role_map = "admins".to_owned();

        let mut nameless = draft("Okta");
        nameless.provider_name = String::new();
        let mut spaced = draft("Okta");
        spaced.groups_claim = "my groups".to_owned();

        // Act / Assert
        assert!(slot.prepare(&plain_http, None, "root").is_err());
        assert!(slot.prepare(&bad_map, None, "root").is_err());
        assert!(slot.prepare(&nameless, None, "root").is_err());
        assert!(slot.prepare(&spaced, None, "root").is_err());
        assert!(
            slot.store.load().await.expect("reads").is_none(),
            "nothing was stored"
        );
    }

    #[test]
    fn a_github_provider_defaults_to_github_dot_com_and_takes_github_scopes_only() {
        // Arrange
        let slot = slot(true);
        let mut github = draft("GitHub");
        github.kind = ProviderKind::GitHub;
        github.issuer = String::new();
        github.scopes = "read:user user:email read:org".to_owned();
        github.client_secret = SecretChange::Set("s3cret".to_owned());
        let mut oidc_scopes = github.clone();
        oidc_scopes.scopes = "openid profile email".to_owned();
        let mut enterprise = github.clone();
        enterprise.issuer = "https://github.example.com".to_owned();

        let mut public = github.clone();
        public.client_secret = SecretChange::Clear;

        // Act
        let (settings, provider) = slot.prepare(&github, None, "root").expect("prepares");
        let refused = slot.prepare(&oidc_scopes, None, "root");
        let (ghes, _) = slot.prepare(&enterprise, None, "root").expect("prepares");
        let secretless = slot.prepare(&public, None, "root");

        // Assert
        assert_eq!(settings.kind, ProviderKind::GitHub);
        assert_eq!(settings.issuer, "https://github.com");
        assert_eq!(provider.issuer(), "https://github.com");
        assert!(
            refused
                .expect_err("openid is no GitHub scope")
                .to_string()
                .contains("read:user")
        );
        assert_eq!(ghes.issuer, "https://github.example.com");
        assert!(
            secretless
                .expect_err("GitHub needs the secret")
                .to_string()
                .contains("client secret")
        );
    }

    #[tokio::test]
    async fn settings_this_instance_cannot_build_stop_the_provider_and_nothing_else() {
        // Arrange: a client secret sealed under one session secret, read by
        // an instance started with another - the rotation case.
        let writer = slot(true);
        let mut confidential = draft("Okta");
        confidential.client_secret = SecretChange::Set("s3cret".to_owned());
        let (settings, provider) = writer
            .prepare(&confidential, None, "root")
            .expect("prepares");
        let saved = writer.store(&settings, provider).await.expect("stores");
        let other = ProviderSlot::new(
            ProviderSettingsStore::memory(),
            Some(
                Sealer::from_secret(b"a different session secret, also long enough")
                    .expect("sealer"),
            ),
            sessions(),
        );
        other
            .store
            .save(&saved)
            .await
            .expect("the same row, seen elsewhere");

        // Act
        let current = other.current().await.expect("not an outage");
        let recent = other.recent().await.expect("not an outage");

        // Assert
        assert!(current.is_none(), "no provider to offer");
        assert!(recent.is_none());
        assert!(
            other
                .trouble()
                .is_some_and(|why| why.contains("OKF_WEB_SESSION_SECRET")),
            "and the Admin page can say why"
        );
    }

    #[tokio::test]
    async fn a_read_that_straddled_a_save_does_not_put_the_older_settings_back() {
        // Arrange: the saver remembered the newer settings first.
        let slot = slot(false);
        let (older, older_provider) = slot
            .prepare(&draft("Okta"), None, "root")
            .expect("prepares");
        let (newer, newer_provider) = slot
            .prepare(&draft("Entra ID"), None, "root")
            .expect("prepares");
        slot.store(&newer, newer_provider).await.expect("stores");

        // Act: a slower read, which had seen the row before the save,
        // arrives with the older settings under a stamp that sorts first.
        let answer = slot.remember(
            older.updated_at.clone(),
            State::Provider(Arc::new(older_provider)),
        );

        // Assert
        assert_eq!(answer.expect("a provider").provider_name(), "Entra ID");
        assert_eq!(
            slot.recent()
                .await
                .expect("reads")
                .expect("a provider")
                .provider_name(),
            "Entra ID"
        );
    }

    #[tokio::test]
    async fn the_request_path_believes_a_provider_briefly_and_then_asks_again() {
        // Arrange: a provider built here, then changed "elsewhere" (the
        // store is shared; the cache is not told).
        let slot = slot(false);
        let (settings, provider) = slot
            .prepare(&draft("Okta"), None, "root")
            .expect("prepares");
        slot.store(&settings, provider).await.expect("stores");
        let believed = slot.recent().await.expect("reads").expect("a provider");
        let (changed, _) = slot
            .prepare(&draft("Entra ID"), Some(&settings), "root")
            .expect("prepares");
        slot.store.save(&changed).await.expect("changed elsewhere");

        // Act
        let still = slot.recent().await.expect("reads").expect("a provider");
        if let Some(live) = slot
            .live
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .as_mut()
        {
            live.at = Instant::now()
                .checked_sub(BELIEVED_FOR)
                .expect("the process has been up for longer than that");
        }
        let later = slot.recent().await.expect("reads").expect("a provider");

        // Assert
        assert!(Arc::ptr_eq(&believed, &still), "believed for a while");
        assert_eq!(later.provider_name(), "Entra ID", "then read again");
    }
}
