// SPDX-License-Identifier: AGPL-3.0-only
//! The identity provider an admin set up on the Admin page, kept in the
//! catalog (`pgokf_web.oidc`, one row) beside the people and sessions.
//!
//! The `users` mode keeps its password sign-in and offers the provider
//! beside it once one is saved here; every UI instance sees the same row,
//! and notices a change through `updated_at`. The client secret is stored
//! sealed (see [`crate::seal`]), so the row holds nothing a writer
//! credential could use at the provider.

#[cfg(test)]
use std::sync::Mutex;

use anyhow::{Context, Result};

use crate::auth::{Role, RoleMapping};
use crate::db::Db;
use crate::oidc::OidcConfig;
use crate::seal::Sealer;

/// One row of `pgokf_web.oidc`, as stored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct OidcSettings {
    pub enabled: bool,
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

impl OidcSettings {
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
pub(crate) enum OidcSettingsStore {
    /// The catalog, through the identity pool.
    Pg(Db),
    /// In memory, for tests (boxed: a row is far larger than a pool handle).
    #[cfg(test)]
    Memory(Box<Mutex<Option<OidcSettings>>>),
}

impl std::fmt::Debug for OidcSettingsStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Pg(_) => f.write_str("OidcSettingsStore::Pg"),
            #[cfg(test)]
            Self::Memory(_) => f.write_str("OidcSettingsStore::Memory"),
        }
    }
}

const COLUMNS: &str = "enabled, issuer, client_id, client_secret, redirect_url, scopes, \
                       subject_claims, groups_claim, provider_name, role_map, default_role, \
                       updated_by";
/// The stamp, to the microsecond: two saves within a second, on two
/// instances, must not look like one.
const STAMP: &str = "to_char(updated_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS.US\"Z\"')";

impl OidcSettingsStore {
    #[cfg(test)]
    pub(crate) fn memory() -> Self {
        Self::Memory(Box::new(Mutex::new(None)))
    }

    /// The row, if an admin has set a provider up.
    ///
    /// # Errors
    ///
    /// The catalog cannot be read, or holds a default role this build does
    /// not know.
    pub(crate) async fn load(&self) -> Result<Option<OidcSettings>> {
        match self {
            Self::Pg(db) => {
                let row = db
                    .query_opt(
                        &format!("SELECT {COLUMNS}, {STAMP} FROM pgokf_web.oidc"),
                        &[],
                    )
                    .await
                    .context("reading the identity provider settings")?;
                row.map(|row| {
                    let default_role: String = row.try_get(10)?;
                    Ok(OidcSettings {
                        enabled: row.try_get(0)?,
                        issuer: row.try_get(1)?,
                        client_id: row.try_get(2)?,
                        client_secret: row.try_get(3)?,
                        redirect_url: row.try_get(4)?,
                        scopes: row.try_get(5)?,
                        subject_claims: row.try_get(6)?,
                        groups_claim: row.try_get(7)?,
                        provider_name: row.try_get(8)?,
                        role_map: row.try_get(9)?,
                        default_role: Role::parse(&default_role).with_context(|| {
                            format!(
                                "the catalog holds a default role {default_role:?} this build \
                                 does not know"
                            )
                        })?,
                        updated_by: row.try_get(11)?,
                        updated_at: row.try_get(12)?,
                    })
                })
                .transpose()
            }
            #[cfg(test)]
            Self::Memory(settings) => Ok(settings.lock().expect("settings lock").clone()),
        }
    }

    /// Store the settings as the one row, stamped by the catalog; what was
    /// stored comes back, stamp included.
    ///
    /// # Errors
    ///
    /// The catalog refuses the row (a constraint) or cannot be written.
    pub(crate) async fn save(&self, settings: &OidcSettings) -> Result<OidcSettings> {
        match self {
            Self::Pg(db) => {
                let row = db
                    .query_one(
                        &format!(
                            "INSERT INTO pgokf_web.oidc ({COLUMNS})
                             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12)
                             ON CONFLICT (singleton) DO UPDATE SET
                                 enabled = EXCLUDED.enabled, issuer = EXCLUDED.issuer,
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
                        ),
                        &[
                            &settings.enabled,
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
                    .context("saving the identity provider settings")?;
                Ok(OidcSettings {
                    updated_at: row.try_get(0)?,
                    ..settings.clone()
                })
            }
            #[cfg(test)]
            Self::Memory(slot) => {
                let mut slot = slot.lock().expect("settings lock");
                let stamp = slot.as_ref().map_or(0, |s| s.updated_at.len()) + 1;
                let saved = OidcSettings {
                    updated_at: "x".repeat(stamp),
                    ..settings.clone()
                };
                *slot = Some(saved.clone());
                Ok(saved)
            }
        }
    }

    /// Forget the provider: `false` when none was set up.
    ///
    /// # Errors
    ///
    /// The catalog cannot be written.
    pub(crate) async fn remove(&self) -> Result<bool> {
        match self {
            Self::Pg(db) => Ok(db
                .execute("DELETE FROM pgokf_web.oidc", &[])
                .await
                .context("removing the identity provider settings")?
                > 0),
            #[cfg(test)]
            Self::Memory(slot) => Ok(slot.lock().expect("settings lock").take().is_some()),
        }
    }
}
