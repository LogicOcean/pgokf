// SPDX-License-Identifier: AGPL-3.0-only
//! The identity providers the `users` mode offers beside its own sign-in,
//! as set up on the Admin page: each built from the catalog's settings,
//! kept while they stand, rebuilt when they change.
//!
//! Settings are read on the paths where a provider is *used to sign in*
//! (the sign-in page, the callback, sign-out) - a few rows, rarely - and
//! never on the request path: a session a provider opened names the
//! provider (its slug), which is recognized by the one already built, and
//! the first such request after a start builds it once. A change saved on
//! any instance is noticed by every other through the row's stamp on its
//! next sign-in.

use std::collections::HashMap;
use std::sync::{Arc, PoisonError, RwLock};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};

use crate::auth::{Role, Sessions};
use crate::oidc::OidcAuth;
use crate::provider_settings::{
    ProviderKind, ProviderSettings, ProviderSettingsStore, free_slug, valid_slug,
};
use crate::seal::Sealer;

/// What the Admin page's form says.
#[derive(Clone)]
pub(crate) struct ProviderDraft {
    /// The slug of the provider being changed; `None` for a new one, which
    /// gets a slug made from its name.
    pub id: Option<String>,
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
    /// it with. Sign-in goes on without this provider, and the Admin page
    /// says why.
    Unbuildable(String),
}

/// The registry the `users` mode keeps its providers in, by slug.
pub(crate) struct ProviderRegistry {
    store: ProviderSettingsStore,
    /// Present when the operator set a session secret: the only key a
    /// client secret can be sealed under.
    sealer: Option<Sealer>,
    sessions: Arc<Sessions>,
    live: RwLock<HashMap<String, Live>>,
}

impl std::fmt::Debug for ProviderRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProviderRegistry")
            .field("store", &self.store)
            .field("can_seal", &self.sealer.is_some())
            .finish_non_exhaustive()
    }
}

impl ProviderRegistry {
    pub(crate) fn new(
        store: ProviderSettingsStore,
        sealer: Option<Sealer>,
        sessions: Arc<Sessions>,
    ) -> Self {
        Self {
            store,
            sealer,
            sessions,
            live: RwLock::new(HashMap::new()),
        }
    }

    /// Whether a client secret could be stored here.
    pub(crate) fn can_seal(&self) -> bool {
        self.sealer.is_some()
    }

    /// Every enabled provider as the catalog has them now, in the order the
    /// sign-in page shows them: each built on first use, kept while its
    /// settings' stamp stands, rebuilt when it moves. One whose settings
    /// cannot be built into a provider here (see [`Self::trouble`]) is left
    /// out, which must not stop the others or the password sign-in.
    ///
    /// # Errors
    ///
    /// The catalog cannot be read.
    pub(crate) async fn all_current(&self) -> Result<Vec<Arc<OidcAuth>>> {
        let all = self.store.load_all().await?;
        // A provider removed or switched off elsewhere is forgotten here,
        // so the request path stops believing it too.
        let enabled: Vec<&ProviderSettings> = all.iter().filter(|s| s.enabled).collect();
        self.live
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .retain(|id, _| enabled.iter().any(|s| s.id == *id));
        Ok(enabled
            .into_iter()
            .filter_map(|settings| self.refresh(settings))
            .collect())
    }

    /// One provider by its slug, as the catalog has it now, or `None`
    /// while it is not set up, not enabled, or cannot be built here.
    ///
    /// # Errors
    ///
    /// The catalog cannot be read.
    pub(crate) async fn current(&self, id: &str) -> Result<Option<Arc<OidcAuth>>> {
        let Some(settings) = self.store.load(id).await?.filter(|s| s.enabled) else {
            self.forget(id);
            return Ok(None);
        };
        Ok(self.refresh(&settings))
    }

    /// The provider `settings` make, from the cache while the stamp stands
    /// and built afresh when it moved.
    fn refresh(&self, settings: &ProviderSettings) -> Option<Arc<OidcAuth>> {
        let known = self
            .live
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&settings.id)
            .filter(|live| live.stamp == settings.updated_at)
            .map(Live::answer);
        if let Some(answer) = known {
            self.touch(&settings.id);
            return answer;
        }
        let state = match self.build(settings) {
            Ok(provider) => State::Provider(Arc::new(provider)),
            Err(error) => {
                // Said once per change of the settings, not per request.
                eprintln!(
                    "pgokf-web: the identity provider {} set up on the Admin page cannot be used \
                     here: {error:#}",
                    settings.provider_name
                );
                State::Unbuildable(format!("{error:#}"))
            }
        };
        self.remember(&settings.id, settings.updated_at.clone(), state)
    }

    /// The provider last built under `id`, for the request path: without
    /// asking the catalog while it was built or confirmed recently, and
    /// read again when it was not - so another instance's change is seen
    /// here within [`BELIEVED_FOR`].
    ///
    /// # Errors
    ///
    /// The catalog cannot be read.
    pub(crate) async fn recent(&self, id: &str) -> Result<Option<Arc<OidcAuth>>> {
        let believed = self
            .live
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .get(id)
            .filter(|live| live.at.elapsed() < BELIEVED_FOR)
            .map(Live::answer);
        match believed {
            Some(answer) => Ok(answer),
            None => self.current(id).await,
        }
    }

    /// Why the stored settings of `id` cannot be used by this instance, if
    /// that is so: shown on the Admin page, where an admin can enter the
    /// client secret again after a session-secret rotation.
    pub(crate) fn trouble(&self, id: &str) -> Option<String> {
        match self
            .live
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .get(id)
            .map(|live| &live.state)
        {
            Some(State::Unbuildable(why)) => Some(why.clone()),
            _ => None,
        }
    }

    /// Every provider's settings as stored, enabled or not, for the Admin
    /// page.
    ///
    /// # Errors
    ///
    /// The catalog cannot be read.
    pub(crate) async fn settings(&self) -> Result<Vec<ProviderSettings>> {
        self.store.load_all().await
    }

    /// One provider's settings as stored, for the Admin page's form.
    ///
    /// # Errors
    ///
    /// The catalog cannot be read.
    pub(crate) async fn setting(&self, id: &str) -> Result<Option<ProviderSettings>> {
        self.store.load(id).await
    }

    /// Turn the form into settings ready to store, and prove they build a
    /// provider - URLs, client id, claims, role map - without touching the
    /// network. `all` is every provider stored now: the one being changed
    /// is found there by the draft's slug (its stored secret kept when the
    /// form left the field blank), and a new one gets a slug none of them
    /// has. The provider comes back so the caller can ask it to reach the
    /// issuer before anything is stored.
    ///
    /// # Errors
    ///
    /// A setting that cannot be right, another provider of the same name,
    /// a slug that names no provider, or a client secret with no session
    /// secret to seal it under.
    pub(crate) fn prepare(
        &self,
        draft: &ProviderDraft,
        all: &[ProviderSettings],
        by: &str,
    ) -> Result<(ProviderSettings, OidcAuth)> {
        let issuer = checked(draft)?;
        let provider_name = draft.provider_name.trim().to_owned();
        let (id, stored) = placed(draft, all, &provider_name)?;
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
            id,
            enabled: draft.enabled,
            kind: draft.kind,
            issuer,
            client_id: draft.client_id.trim().to_owned(),
            client_secret,
            redirect_url: draft.redirect_url.trim().to_owned(),
            scopes: draft.scopes.trim().to_owned(),
            subject_claims: draft.subject_claims.trim().to_owned(),
            groups_claim: draft.groups_claim.trim().to_owned(),
            provider_name,
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
    /// The catalog refuses the row (a name or handle already taken) or
    /// cannot be written.
    pub(crate) async fn store(
        &self,
        settings: &ProviderSettings,
        provider: OidcAuth,
        is_new: bool,
    ) -> Result<ProviderSettings> {
        let saved = self.store.save(settings, is_new).await?;
        if saved.enabled {
            self.remember(
                &saved.id,
                saved.updated_at.clone(),
                State::Provider(Arc::new(provider)),
            );
        } else {
            self.forget(&saved.id);
        }
        Ok(saved)
    }

    /// Forget a provider altogether: `false` when none was set up as `id`.
    ///
    /// # Errors
    ///
    /// The catalog cannot be written.
    pub(crate) async fn remove(&self, id: &str) -> Result<bool> {
        let removed = self.store.remove(id).await?;
        self.forget(id);
        Ok(removed)
    }

    fn build(&self, settings: &ProviderSettings) -> Result<OidcAuth> {
        OidcAuth::new(
            settings.config(self.sealer.as_ref())?,
            Arc::clone(&self.sessions),
        )
    }

    /// Keep what a read of `id`'s settings came to. A read that straddled a
    /// save - it saw the row before, and gets here after the saver
    /// remembered the newer settings - must not put the older ones back:
    /// the stamps order (an instant, to the microsecond), so a newer entry
    /// believed within [`BELIEVED_FOR`] stands.
    fn remember(&self, id: &str, stamp: String, state: State) -> Option<Arc<OidcAuth>> {
        let mut live = self.live.write().unwrap_or_else(PoisonError::into_inner);
        if let Some(newer) = live
            .get(id)
            .filter(|known| known.stamp > stamp && known.at.elapsed() < BELIEVED_FOR)
        {
            return newer.answer();
        }
        let entry = Live {
            stamp,
            at: Instant::now(),
            state,
        };
        let answer = entry.answer();
        live.insert(id.to_owned(), entry);
        answer
    }

    /// The settings of `id` were read again and stand: believed afresh.
    fn touch(&self, id: &str) {
        if let Some(live) = self
            .live
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .get_mut(id)
        {
            live.at = Instant::now();
        }
    }

    fn forget(&self, id: &str) {
        self.live
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(id);
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

/// The form's fields against the catalog's own constraints, checked here
/// first so a slip is a message on the form rather than a refused row; the
/// issuer comes back as it will be stored.
fn checked(draft: &ProviderDraft) -> Result<String> {
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
    Ok(issuer)
}

/// The slug the draft is stored under and, for a change, the row as it is
/// now - among `all`, where the name must be the draft's alone.
fn placed<'a>(
    draft: &ProviderDraft,
    all: &'a [ProviderSettings],
    provider_name: &str,
) -> Result<(String, Option<&'a ProviderSettings>)> {
    let (id, stored) = if let Some(id) = draft
        .id
        .as_deref()
        .map(str::trim)
        .filter(|id| !id.is_empty())
    {
        let stored = all
            .iter()
            .find(|s| s.id == id)
            .with_context(|| format!("no identity provider is set up as {id}"))?;
        (id.to_owned(), Some(stored))
    } else {
        let taken: Vec<String> = all.iter().map(|s| s.id.clone()).collect();
        (free_slug(provider_name, draft.kind, &taken), None)
    };
    if !valid_slug(&id) {
        bail!("{id:?} is not a provider slug");
    }
    if let Some(other) = all
        .iter()
        .find(|s| s.id != id && s.provider_name.eq_ignore_ascii_case(provider_name))
    {
        bail!(
            "another provider is already called {}: give this one a name of its own",
            other.provider_name
        );
    }
    Ok((id, stored))
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

    fn registry(sealer: bool) -> ProviderRegistry {
        ProviderRegistry::new(
            ProviderSettingsStore::memory(),
            sealer.then(|| Sealer::from_secret(SECRET.as_bytes()).expect("sealer")),
            sessions(),
        )
    }

    fn draft(name: &str) -> ProviderDraft {
        ProviderDraft {
            id: None,
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

    /// The form filled in to change the provider stored as `stored`.
    fn change(stored: &ProviderSettings, name: &str) -> ProviderDraft {
        ProviderDraft {
            id: Some(stored.id.clone()),
            ..draft(name)
        }
    }

    #[tokio::test]
    async fn a_provider_is_built_once_and_rebuilt_when_the_settings_change() {
        // Arrange
        let registry = registry(false);
        let (settings, settings_provider) = registry
            .prepare(&draft("Okta"), &[], "root")
            .expect("prepares");
        let stored = registry
            .store(&settings, settings_provider, true)
            .await
            .expect("stores");

        // Act
        let first = registry
            .current("okta")
            .await
            .expect("reads")
            .expect("a provider");
        let again = registry
            .current("okta")
            .await
            .expect("reads")
            .expect("a provider");
        let (changed, changed_provider) = registry
            .prepare(
                &change(&stored, "Entra ID"),
                std::slice::from_ref(&stored),
                "root",
            )
            .expect("prepares");
        registry
            .store(&changed, changed_provider, false)
            .await
            .expect("stores");
        let rebuilt = registry
            .current("okta")
            .await
            .expect("reads")
            .expect("a provider");

        // Assert
        assert_eq!(stored.id, "okta", "a slug made from the name");
        assert!(Arc::ptr_eq(&first, &again), "kept while the stamp stands");
        assert_eq!(first.provider_name(), "Okta");
        assert_eq!(first.stored_id(), Some("okta"));
        assert!(!Arc::ptr_eq(&first, &rebuilt), "rebuilt when it moves");
        assert_eq!(rebuilt.provider_name(), "Entra ID");
        assert_eq!(changed.id, "okta", "the slug survives a rename");
        assert!(
            registry
                .recent("okta")
                .await
                .expect("reads")
                .is_some_and(|p| Arc::ptr_eq(&p, &rebuilt))
        );
    }

    #[tokio::test]
    async fn every_provider_has_its_own_slug_and_name() {
        // Arrange
        let registry = registry(true);
        let (okta, okta_provider) = registry
            .prepare(&draft("Okta"), &[], "root")
            .expect("prepares");
        let okta = registry
            .store(&okta, okta_provider, true)
            .await
            .expect("stores");
        let mut github = draft("GitHub");
        github.kind = ProviderKind::GitHub;
        github.issuer = String::new();
        github.scopes = "read:user".to_owned();
        github.client_secret = SecretChange::Set("s3cret".to_owned());
        let (github, github_provider) = registry
            .prepare(&github, std::slice::from_ref(&okta), "root")
            .expect("prepares");
        let github = registry
            .store(&github, github_provider, true)
            .await
            .expect("stores");
        let all = registry.settings().await.expect("reads");

        // Act
        let same_name = registry.prepare(&draft("OKTA"), &all, "root");
        let (same_slug, _) = registry
            .prepare(&draft("Okta!"), &all, "root")
            .expect("a name of its own, though the slug collides");
        let unknown = registry.prepare(
            &ProviderDraft {
                id: Some("nobody".to_owned()),
                ..draft("Nobody")
            },
            &all,
            "root",
        );
        let offered = registry.all_current().await.expect("reads");

        // Assert
        assert_eq!(github.id, "github");
        assert!(
            same_name
                .expect_err("two buttons with one label")
                .to_string()
                .contains("already called Okta")
        );
        assert_eq!(same_slug.id, "okta-2");
        assert!(unknown.is_err());
        assert_eq!(
            offered
                .iter()
                .map(|p| p.provider_name())
                .collect::<Vec<_>>(),
            ["GitHub", "Okta"],
            "by name"
        );
    }

    #[tokio::test]
    async fn a_disabled_or_absent_provider_is_not_offered() {
        // Arrange
        let registry = registry(false);
        let none = registry.current("okta").await.expect("reads");
        let mut off = draft("Okta");
        off.enabled = false;
        let (settings, settings_provider) = registry.prepare(&off, &[], "root").expect("prepares");
        registry
            .store(&settings, settings_provider, true)
            .await
            .expect("stores");

        // Act
        let disabled = registry.current("okta").await.expect("reads");
        let offered = registry.all_current().await.expect("reads");
        let removed = registry.remove("okta").await.expect("removes");

        // Assert
        assert!(none.is_none());
        assert!(disabled.is_none());
        assert!(offered.is_empty());
        assert!(registry.recent("okta").await.expect("reads").is_none());
        assert!(removed);
        assert!(
            !registry.remove("okta").await.expect("answers"),
            "nothing left to remove"
        );
    }

    #[tokio::test]
    async fn a_client_secret_needs_a_session_secret_and_is_stored_sealed() {
        // Arrange
        let without = registry(false);
        let with = registry(true);
        let mut confidential = draft("Okta");
        confidential.client_secret = SecretChange::Set("s3cret".to_owned());

        // Act
        let refused = without.prepare(&confidential, &[], "root");
        let public = without.prepare(&draft("Okta"), &[], "root");
        let (sealed, provider) = with.prepare(&confidential, &[], "root").expect("prepares");

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
        let registry = registry(true);
        let mut confidential = draft("Okta");
        confidential.client_secret = SecretChange::Set("s3cret".to_owned());
        let (stored, stored_provider) = registry
            .prepare(&confidential, &[], "root")
            .expect("prepares");
        let stored = registry
            .store(&stored, stored_provider, true)
            .await
            .expect("stores");
        let all = [stored.clone()];

        // Act
        let mut renamed = change(&stored, "Okta again");
        renamed.client_secret = SecretChange::Keep;
        let (kept, _) = registry.prepare(&renamed, &all, "root").expect("prepares");
        let (cleared, _) = registry
            .prepare(&change(&stored, "Okta"), &all, "root")
            .expect("prepares");

        // Assert
        assert_eq!(kept.client_secret, stored.client_secret);
        assert_eq!(cleared.client_secret, None);
    }

    #[tokio::test]
    async fn a_setting_that_cannot_be_right_is_refused_before_anything_is_stored() {
        // Arrange
        let registry = registry(false);
        let mut plain_http = draft("Okta");
        plain_http.issuer = "http://id.example".to_owned();
        let mut bad_map = draft("Okta");
        bad_map.role_map = "admins".to_owned();

        let mut nameless = draft("Okta");
        nameless.provider_name = String::new();
        let mut spaced = draft("Okta");
        spaced.groups_claim = "my groups".to_owned();

        // Act / Assert
        assert!(registry.prepare(&plain_http, &[], "root").is_err());
        assert!(registry.prepare(&bad_map, &[], "root").is_err());
        assert!(registry.prepare(&nameless, &[], "root").is_err());
        assert!(registry.prepare(&spaced, &[], "root").is_err());
        assert!(
            registry.store.load_all().await.expect("reads").is_empty(),
            "nothing was stored"
        );
    }

    #[test]
    fn a_github_provider_defaults_to_github_dot_com_and_takes_github_scopes_only() {
        // Arrange
        let registry = registry(true);
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
        let (settings, provider) = registry.prepare(&github, &[], "root").expect("prepares");
        let refused = registry.prepare(&oidc_scopes, &[], "root");
        let (ghes, _) = registry
            .prepare(&enterprise, &[], "root")
            .expect("prepares");
        let secretless = registry.prepare(&public, &[], "root");

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
    async fn settings_this_instance_cannot_build_stop_that_provider_and_nothing_else() {
        // Arrange: a client secret sealed under one session secret, read by
        // an instance started with another - the rotation case - beside a
        // public client that needs no secret.
        let writer = registry(true);
        let mut confidential = draft("Okta");
        confidential.client_secret = SecretChange::Set("s3cret".to_owned());
        let (settings, provider) = writer
            .prepare(&confidential, &[], "root")
            .expect("prepares");
        let saved = writer
            .store(&settings, provider, true)
            .await
            .expect("stores");
        let (public, public_provider) = writer
            .prepare(&draft("Entra"), std::slice::from_ref(&saved), "root")
            .expect("prepares");
        let public = writer
            .store(&public, public_provider, true)
            .await
            .expect("stores");
        let other = ProviderRegistry::new(
            ProviderSettingsStore::memory(),
            Some(
                Sealer::from_secret(b"a different session secret, also long enough")
                    .expect("sealer"),
            ),
            sessions(),
        );
        for row in [&saved, &public] {
            other
                .store
                .save(row, true)
                .await
                .expect("the same rows, seen elsewhere");
        }

        // Act
        let current = other.current("okta").await.expect("not an outage");
        let recent = other.recent("okta").await.expect("not an outage");
        let offered = other.all_current().await.expect("not an outage");

        // Assert
        assert!(current.is_none(), "no provider to offer");
        assert!(recent.is_none());
        assert_eq!(
            offered
                .iter()
                .map(|p| p.provider_name())
                .collect::<Vec<_>>(),
            ["Entra"],
            "the other provider goes on"
        );
        assert!(
            other
                .trouble("okta")
                .is_some_and(|why| why.contains("OKF_WEB_SESSION_SECRET")),
            "and the Admin page can say why"
        );
        assert!(other.trouble("entra").is_none());
    }

    #[tokio::test]
    async fn a_read_that_straddled_a_save_does_not_put_the_older_settings_back() {
        // Arrange: the saver remembered the newer settings first.
        let registry = registry(false);
        let (older, older_provider) = registry
            .prepare(&draft("Okta"), &[], "root")
            .expect("prepares");
        let older = registry
            .store(&older, older_provider, true)
            .await
            .expect("stores");
        let (newer, newer_provider) = registry
            .prepare(
                &change(&older, "Entra ID"),
                std::slice::from_ref(&older),
                "root",
            )
            .expect("prepares");
        registry
            .store(&newer, newer_provider, false)
            .await
            .expect("stores");
        let (older_again, older_again_provider) = registry
            .prepare(
                &change(&older, "Okta"),
                std::slice::from_ref(&older),
                "root",
            )
            .expect("prepares");
        drop(older_again);

        // Act: a slower read, which had seen the row before the save,
        // arrives with the older settings under a stamp that sorts first.
        let answer = registry.remember(
            "okta",
            older.updated_at.clone(),
            State::Provider(Arc::new(older_again_provider)),
        );

        // Assert
        assert_eq!(answer.expect("a provider").provider_name(), "Entra ID");
        assert_eq!(
            registry
                .recent("okta")
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
        let registry = registry(false);
        let (settings, provider) = registry
            .prepare(&draft("Okta"), &[], "root")
            .expect("prepares");
        let stored = registry
            .store(&settings, provider, true)
            .await
            .expect("stores");
        let believed = registry
            .recent("okta")
            .await
            .expect("reads")
            .expect("a provider");
        let (changed, _) = registry
            .prepare(
                &change(&stored, "Entra ID"),
                std::slice::from_ref(&stored),
                "root",
            )
            .expect("prepares");
        registry
            .store
            .save(&changed, false)
            .await
            .expect("changed elsewhere");

        // Act
        let still = registry
            .recent("okta")
            .await
            .expect("reads")
            .expect("a provider");
        if let Some(live) = registry
            .live
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .get_mut("okta")
        {
            live.at = Instant::now()
                .checked_sub(BELIEVED_FOR)
                .expect("the process has been up for longer than that");
        }
        let later = registry
            .recent("okta")
            .await
            .expect("reads")
            .expect("a provider");

        // Assert
        assert!(Arc::ptr_eq(&believed, &still), "believed for a while");
        assert_eq!(later.provider_name(), "Entra ID", "then read again");
    }
}
