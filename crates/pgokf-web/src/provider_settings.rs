// SPDX-License-Identifier: AGPL-3.0-only
//! The identity providers an admin set up on the Admin page, kept in the
//! catalog (`pgokf_web.identity_providers`, one row each) beside the people
//! and sessions: `OpenID` Connect providers, or GitHub.
//!
//! The `users` mode keeps its password sign-in and offers every enabled
//! provider beside it, each by its own button; every UI instance sees the
//! same rows, and notices a change through `updated_at`. The client secret
//! is stored sealed (see [`crate::seal`]), so a row holds nothing a writer
//! credential could use at the provider.
//!
//! A provider is known by a short slug made from its name when it was
//! added (`github`, `okta`, `okta-2`): the handle in the sign-in URL, on
//! the sessions it opens, and on the people it signed in. It never changes,
//! even when the name does.

#[cfg(test)]
use std::collections::BTreeMap;
#[cfg(test)]
use std::sync::Mutex;

use anyhow::{Context, Result};
use tokio_postgres::error::SqlState;

use crate::auth::{Role, RoleMapping};
use crate::db::{Db, sql_state};
use crate::oidc::OidcConfig;
use crate::seal::Sealer;

/// What a provider speaks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ProviderKind {
    /// `OpenID` Connect: discovery, and an ID token verified against the
    /// provider's published keys.
    Oidc,
    /// GitHub's OAuth web flow (github.com or a GitHub Enterprise Server):
    /// the person from `/user`, their verified email from `/user/emails`,
    /// their groups from the organizations and `org/team` slugs they
    /// belong to. GitHub speaks no `OpenID` Connect.
    GitHub,
}

impl ProviderKind {
    /// Every kind, as the form lists them.
    pub(crate) const fn all() -> &'static [ProviderKind] {
        &[ProviderKind::Oidc, ProviderKind::GitHub]
    }

    /// The kind as the catalog stores it.
    pub(crate) const fn id(self) -> &'static str {
        match self {
            ProviderKind::Oidc => "oidc",
            ProviderKind::GitHub => "github",
        }
    }

    /// What the form calls it.
    pub(crate) const fn label(self) -> &'static str {
        match self {
            ProviderKind::Oidc => "OpenID Connect",
            ProviderKind::GitHub => "GitHub",
        }
    }

    pub(crate) fn parse(id: &str) -> Option<Self> {
        let id = id.trim();
        Self::all().iter().copied().find(|kind| kind.id() == id)
    }
}

/// The longest slug a provider may have (what the catalog's constraint
/// allows).
pub(crate) const SLUG_MAX: usize = 32;

/// Whether `id` is a provider slug as the catalog constrains it: lowercase
/// letters, digits, and dashes, starting with a letter or digit.
pub(crate) fn valid_slug(id: &str) -> bool {
    let mut chars = id.chars();
    chars
        .next()
        .is_some_and(|first| first.is_ascii_lowercase() || first.is_ascii_digit())
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        && id.len() <= SLUG_MAX
}

/// The slug a provider name makes: its letters and digits, lowercased,
/// with every other run of characters a single dash - `GitHub` is
/// `github`, `Entra ID (staff)` is `entra-id-staff`. A name with nothing
/// usable in it falls back to the kind.
pub(crate) fn slug_for(name: &str, kind: ProviderKind) -> String {
    let mut slug = String::new();
    let mut pending_dash = false;
    for c in name.chars() {
        if c.is_ascii_alphanumeric() {
            // The dash and the character together must not overshoot the
            // bound - a slug one over is refused by `valid_slug`.
            let want = usize::from(pending_dash && !slug.is_empty()) + 1;
            if slug.len() + want > SLUG_MAX {
                break;
            }
            if pending_dash && !slug.is_empty() {
                slug.push('-');
            }
            pending_dash = false;
            slug.push(c.to_ascii_lowercase());
        } else {
            pending_dash = true;
        }
    }
    let slug = slug.trim_end_matches('-').to_owned();
    if slug.is_empty() {
        kind.id().to_owned()
    } else {
        slug
    }
}

/// A slug for `name` that none of `taken` has: the plain one, else
/// `-2`, `-3`, ... after it.
pub(crate) fn free_slug(name: &str, kind: ProviderKind, taken: &[String]) -> String {
    let base = slug_for(name, kind);
    if !taken.contains(&base) {
        return base;
    }
    (2..=usize::MAX)
        .map(|n| {
            let suffix = format!("-{n}");
            let keep = base.len().min(SLUG_MAX - suffix.len());
            format!("{}{suffix}", base[..keep].trim_end_matches('-'))
        })
        .find(|candidate| !taken.contains(candidate))
        .expect("more numbers than providers")
}

/// One row of `pgokf_web.identity_providers`, as stored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ProviderSettings {
    /// The slug (see the module doc).
    pub id: String,
    pub enabled: bool,
    pub kind: ProviderKind,
    pub issuer: String,
    pub client_id: String,
    /// The client secret in its sealed form, or `None` for a public client.
    pub client_secret: Option<String>,
    pub redirect_url: String,
    pub scopes: String,
    /// Comma-separated, tried in order.
    pub subject_claims: String,
    pub groups_claim: String,
    pub provider_name: String,
    /// `group=role` entries, comma-separated.
    pub role_map: String,
    pub default_role: Role,
    /// When the row last changed, as an ISO 8601 instant: the stamp every
    /// instance compares to know whether its provider is current.
    pub updated_at: String,
    pub updated_by: String,
}

impl ProviderSettings {
    /// What the provider needs, with the client secret opened.
    ///
    /// # Errors
    ///
    /// A stored secret with no sealer to open it (this instance has no
    /// session secret), a secret sealed under another session secret, or
    /// a role map that does not parse.
    pub(crate) fn config(&self, sealer: Option<&Sealer>) -> Result<OidcConfig> {
        let client_secret = match &self.client_secret {
            Some(ciphertext) => Some(
                sealer
                    .context(
                        "a client secret is stored but this instance has no \
                         OKF_WEB_SESSION_SECRET to open it with",
                    )?
                    .open(ciphertext)?,
            ),
            None => None,
        };
        Ok(OidcConfig {
            id: Some(self.id.clone()),
            kind: self.kind,
            issuer: self.issuer.clone(),
            client_id: self.client_id.clone(),
            client_secret,
            redirect_uri: self.redirect_url.clone(),
            scopes: self.scopes.clone(),
            subject_claims: self
                .subject_claims
                .split(',')
                .map(str::trim)
                .filter(|c| !c.is_empty())
                .map(str::to_owned)
                .collect(),
            groups_claim: self.groups_claim.trim().to_owned(),
            roles: RoleMapping::parse(&self.role_map, self.default_role)?,
            provider_name: self.provider_name.trim().to_owned(),
        })
    }
}

/// Where the settings are kept.
pub(crate) enum ProviderSettingsStore {
    /// The catalog, through the identity pool.
    Pg(Db),
    /// In memory, for tests (boxed: rows are far larger than a pool handle).
    #[cfg(test)]
    Memory(Box<Mutex<BTreeMap<String, ProviderSettings>>>),
}

impl std::fmt::Debug for ProviderSettingsStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Pg(_) => f.write_str("ProviderSettingsStore::Pg"),
            #[cfg(test)]
            Self::Memory(_) => f.write_str("ProviderSettingsStore::Memory"),
        }
    }
}

const COLUMNS: &str = "id, enabled, kind, issuer, client_id, client_secret, redirect_url, scopes, \
                       subject_claims, groups_claim, provider_name, role_map, default_role, \
                       updated_by";
/// The stamp, to the microsecond: two saves within a second, on two
/// instances, must not look like one.
const STAMP: &str = "to_char(updated_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS.US\"Z\"')";
/// How the rows are listed: by name, as the sign-in page shows them.
const ORDER: &str = "ORDER BY lower(provider_name), id";

impl ProviderSettingsStore {
    #[cfg(test)]
    pub(crate) fn memory() -> Self {
        Self::Memory(Box::new(Mutex::new(BTreeMap::new())))
    }

    /// Every provider an admin has set up, by name.
    ///
    /// # Errors
    ///
    /// The catalog cannot be read, or holds a kind or default role this
    /// build does not know.
    pub(crate) async fn load_all(&self) -> Result<Vec<ProviderSettings>> {
        match self {
            Self::Pg(db) => db
                .query(
                    &format!("SELECT {COLUMNS}, {STAMP} FROM pgokf_web.identity_providers {ORDER}"),
                    &[],
                )
                .await
                .context("reading the identity providers")?
                .iter()
                .map(settings_from)
                .collect(),
            #[cfg(test)]
            Self::Memory(rows) => {
                let mut all: Vec<ProviderSettings> = rows
                    .lock()
                    .expect("settings lock")
                    .values()
                    .cloned()
                    .collect();
                all.sort_by(|a, b| {
                    a.provider_name
                        .to_lowercase()
                        .cmp(&b.provider_name.to_lowercase())
                        .then_with(|| a.id.cmp(&b.id))
                });
                Ok(all)
            }
        }
    }

    /// One provider's row, by slug.
    ///
    /// # Errors
    ///
    /// The catalog cannot be read, or holds a kind or default role this
    /// build does not know.
    pub(crate) async fn load(&self, id: &str) -> Result<Option<ProviderSettings>> {
        match self {
            Self::Pg(db) => db
                .query_opt(
                    &format!(
                        "SELECT {COLUMNS}, {STAMP} FROM pgokf_web.identity_providers WHERE id = $1"
                    ),
                    &[&id],
                )
                .await
                .context("reading an identity provider's settings")?
                .as_ref()
                .map(settings_from)
                .transpose(),
            #[cfg(test)]
            Self::Memory(rows) => Ok(rows.lock().expect("settings lock").get(id).cloned()),
        }
    }

    /// Store the settings as the row of their slug - a new one, or the
    /// existing one changed - stamped by the catalog; what was stored comes
    /// back, stamp included.
    ///
    /// # Errors
    ///
    /// The catalog refuses the row (a constraint: another provider of that
    /// name, say) or cannot be written.
    pub(crate) async fn save(
        &self,
        settings: &ProviderSettings,
        is_new: bool,
    ) -> Result<ProviderSettings> {
        match self {
            Self::Pg(db) => {
                // A new provider is a plain INSERT: the id or the name
                // already taken (a slug reused, or two admins racing) is a
                // unique violation, not a silent overwrite of the other
                // registration. A change to one is an UPDATE of its row.
                let statement = if is_new {
                    format!(
                        "INSERT INTO pgokf_web.identity_providers ({COLUMNS})
                         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14)
                         RETURNING {STAMP}"
                    )
                } else {
                    format!(
                        "INSERT INTO pgokf_web.identity_providers ({COLUMNS})
                         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14)
                         ON CONFLICT (id) DO UPDATE SET
                             enabled = EXCLUDED.enabled, kind = EXCLUDED.kind,
                             issuer = EXCLUDED.issuer,
                             client_id = EXCLUDED.client_id,
                             client_secret = EXCLUDED.client_secret,
                             redirect_url = EXCLUDED.redirect_url, scopes = EXCLUDED.scopes,
                             subject_claims = EXCLUDED.subject_claims,
                             groups_claim = EXCLUDED.groups_claim,
                             provider_name = EXCLUDED.provider_name,
                             role_map = EXCLUDED.role_map,
                             default_role = EXCLUDED.default_role,
                             updated_at = now(), updated_by = EXCLUDED.updated_by
                         RETURNING {STAMP}"
                    )
                };
                let row = db
                    .query_one(
                        &statement,
                        &[
                            &settings.id,
                            &settings.enabled,
                            &settings.kind.id(),
                            &settings.issuer,
                            &settings.client_id,
                            &settings.client_secret,
                            &settings.redirect_url,
                            &settings.scopes,
                            &settings.subject_claims,
                            &settings.groups_claim,
                            &settings.provider_name,
                            &settings.role_map,
                            &settings.default_role.id(),
                            &settings.updated_by,
                        ],
                    )
                    .await
                    .map_err(|error| match sql_state(&error) {
                        Some(SqlState::UNIQUE_VIOLATION) => anyhow::anyhow!(
                            "another provider already has that name or handle; give this one a                              name of its own"
                        ),
                        _ => error.context("saving an identity provider's settings"),
                    })?;
                Ok(ProviderSettings {
                    updated_at: row.try_get(0)?,
                    ..settings.clone()
                })
            }
            #[cfg(test)]
            Self::Memory(rows) => {
                let mut rows = rows.lock().expect("settings lock");
                if is_new && rows.contains_key(&settings.id) {
                    anyhow::bail!("a provider is already set up as {}", settings.id);
                }
                if rows.values().any(|other| {
                    other.id != settings.id
                        && other
                            .provider_name
                            .eq_ignore_ascii_case(&settings.provider_name)
                }) {
                    anyhow::bail!(
                        "saving an identity provider's settings: another provider is called {}",
                        settings.provider_name
                    );
                }
                let stamp = rows.values().map(|s| s.updated_at.len()).max().unwrap_or(0) + 1;
                let saved = ProviderSettings {
                    updated_at: "x".repeat(stamp),
                    ..settings.clone()
                };
                rows.insert(saved.id.clone(), saved.clone());
                Ok(saved)
            }
        }
    }

    /// Forget a provider: `false` when there was none by that slug.
    ///
    /// # Errors
    ///
    /// The catalog cannot be written.
    pub(crate) async fn remove(&self, id: &str) -> Result<bool> {
        match self {
            Self::Pg(db) => Ok(db
                .execute(
                    "DELETE FROM pgokf_web.identity_providers WHERE id = $1",
                    &[&id],
                )
                .await
                .context("removing an identity provider")?
                > 0),
            #[cfg(test)]
            Self::Memory(rows) => Ok(rows.lock().expect("settings lock").remove(id).is_some()),
        }
    }
}

/// One row as [`COLUMNS`] then [`STAMP`] select it.
fn settings_from(row: &tokio_postgres::Row) -> Result<ProviderSettings> {
    let kind: String = row.try_get(2)?;
    let default_role: String = row.try_get(12)?;
    Ok(ProviderSettings {
        id: row.try_get(0)?,
        enabled: row.try_get(1)?,
        kind: ProviderKind::parse(&kind).with_context(|| {
            format!(
                "the catalog holds a provider of kind {kind:?}, which this build does not speak"
            )
        })?,
        issuer: row.try_get(3)?,
        client_id: row.try_get(4)?,
        client_secret: row.try_get(5)?,
        redirect_url: row.try_get(6)?,
        scopes: row.try_get(7)?,
        subject_claims: row.try_get(8)?,
        groups_claim: row.try_get(9)?,
        provider_name: row.try_get(10)?,
        role_map: row.try_get(11)?,
        default_role: Role::parse(&default_role).with_context(|| {
            format!("the catalog holds a default role {default_role:?} this build does not know")
        })?,
        updated_by: row.try_get(13)?,
        updated_at: row.try_get(14)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_name_makes_a_slug_of_its_letters_and_digits() {
        // Arrange
        let long = "x".repeat(80);
        let cut = "x".repeat(32);
        let names = [
            ("GitHub", "github"),
            ("Entra ID (staff)", "entra-id-staff"),
            ("  Okta -- prod  ", "okta-prod"),
            ("日本語", "oidc"),
            (long.as_str(), cut.as_str()),
        ];

        // Act + Assert
        for (name, expected) in names {
            assert_eq!(slug_for(name, ProviderKind::Oidc), expected, "{name:?}");
            assert!(valid_slug(&slug_for(name, ProviderKind::Oidc)), "{name:?}");
        }
        assert!(!valid_slug("-lead"));
        assert!(!valid_slug("Upper"));
        assert!(!valid_slug(""));
    }

    #[test]
    fn a_taken_slug_gets_a_number() {
        // Arrange
        let taken = ["okta".to_owned(), "okta-2".to_owned()];
        let long = "y".repeat(32);

        // Act
        let free = free_slug("Okta", ProviderKind::Oidc, &taken);
        let first = free_slug("GitHub", ProviderKind::GitHub, &taken);
        let trimmed = free_slug(&long, ProviderKind::Oidc, std::slice::from_ref(&long));

        // Assert
        assert_eq!(free, "okta-3");
        assert_eq!(first, "github");
        assert_eq!(trimmed, format!("{}-2", "y".repeat(30)));
        assert!(valid_slug(&trimmed));
    }
}
