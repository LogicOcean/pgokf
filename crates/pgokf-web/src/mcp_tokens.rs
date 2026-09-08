// SPDX-License-Identifier: AGPL-3.0-only
//! The bearer tokens that let an MCP client call `pgokf-mcp` over HTTP,
//! minted here and kept in the catalog (`pgokf_web.mcp_tokens`) as digests.
//!
//! The Admin page (and `pgokf-web mcp-token`) is where an operator makes
//! one: the token is shown once, on the page that minted it, and only its
//! SHA-256 digest is stored - so a token can be named, listed, and revoked
//! here but never read back. `pgokf-mcp` authenticates a request by asking
//! the catalog whose digest it holds (`pgokf.mcp_token_bearer`), so a token
//! revoked here is refused there with the next request. The table is
//! `pgokf_writer`'s and is reached through the identity pool - small, and
//! never behind the human workflow's minute-long resyncs, so the Admin page
//! answers while a bundle rebuilds.
//!
//! A token is minted for the tenant this UI serves (its `--tenant`, or none),
//! and the MCP server admits only tokens minted for the tenant *it* serves:
//! one process serves one tenant, with tokens of its own. This UI lists and
//! revokes its own tenant's tokens and no other's, and a name is unique
//! within a tenant, so two tenants' admins never see or squat each other's.

#[cfg(test)]
use std::sync::Mutex;

use anyhow::{Context, Result};
use pgokf_companion::mcp_token::{Role, digest_of, new_token, valid_token_name};
use tokio_postgres::error::SqlState;

use crate::db::{Db, iso, sql_state};

/// One token as the Admin page lists it: everything but the token.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct McpToken {
    pub name: String,
    pub role: Role,
    /// The tenant it was minted for, if the UI served one.
    pub tenant: Option<String>,
    /// The admin's subject, or `cli`.
    pub created_by: String,
    /// When it was minted, as an ISO 8601 instant in UTC.
    pub created_at: String,
}

/// Where tokens are kept (see the module doc).
pub(crate) enum McpTokenStore {
    /// The catalog, through the identity pool.
    Pg(Db),
    /// In memory, for tests: each token beside its digest, shared so two
    /// UIs can be put over one store.
    #[cfg(test)]
    Memory(std::sync::Arc<Mutex<Vec<(McpToken, String)>>>),
}

impl std::fmt::Debug for McpTokenStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Pg(_) => f.write_str("McpTokenStore::Pg"),
            #[cfg(test)]
            Self::Memory(_) => f.write_str("McpTokenStore::Memory"),
        }
    }
}

impl McpTokenStore {
    #[cfg(test)]
    pub(crate) fn memory() -> Self {
        Self::Memory(std::sync::Arc::new(Mutex::new(Vec::new())))
    }

    /// Another handle on the same memory store.
    #[cfg(test)]
    fn shared(&self) -> Self {
        match self {
            Self::Pg(db) => Self::Pg(db.clone()),
            Self::Memory(tokens) => Self::Memory(std::sync::Arc::clone(tokens)),
        }
    }

    /// Every token of `tenant`, newest first.
    async fn list(&self, tenant: Option<&str>) -> Result<Vec<McpToken>> {
        match self {
            Self::Pg(db) => {
                let rows = db
                    .query(
                        &format!(
                            "SELECT name, role, tenant, created_by, {}
                             FROM pgokf_web.mcp_tokens
                             WHERE tenant IS NOT DISTINCT FROM $1
                             ORDER BY created_at DESC, name",
                            iso("created_at")
                        ),
                        &[&tenant],
                    )
                    .await
                    .context("listing the MCP tokens")?;
                rows.iter()
                    .map(|row| {
                        let role: String = row.try_get(1)?;
                        Ok(McpToken {
                            name: row.try_get(0)?,
                            role: Role::parse(&role).with_context(|| {
                                format!(
                                    "the catalog holds an MCP token with the role {role:?}, which \
                                     this build does not know"
                                )
                            })?,
                            tenant: row.try_get(2)?,
                            created_by: row.try_get(3)?,
                            created_at: row.try_get(4)?,
                        })
                    })
                    .collect()
            }
            #[cfg(test)]
            Self::Memory(tokens) => Ok(tokens
                .lock()
                .expect("tokens lock")
                .iter()
                .filter(|(token, _)| token.tenant.as_deref() == tenant)
                .map(|(token, _)| token.clone())
                .collect()),
        }
    }

    /// Record a token's digest under its tenant and name: the row as the
    /// catalog stamped it, or `None` when the name is taken within the
    /// tenant. (The digest is unique too, but two random 256-bit values do
    /// not collide, so a unique violation means the name.)
    async fn insert(&self, token: &McpToken, digest: &str) -> Result<Option<McpToken>> {
        match self {
            Self::Pg(db) => {
                let outcome = db
                    .query_opt(
                        &format!(
                            "INSERT INTO pgokf_web.mcp_tokens (name, role, tenant, digest, created_by)
                             VALUES ($1, $2, $3, $4, $5)
                             RETURNING {}",
                            iso("created_at")
                        ),
                        &[
                            &token.name,
                            &token.role.id(),
                            &token.tenant,
                            &digest,
                            &token.created_by,
                        ],
                    )
                    .await;
                match outcome {
                    Ok(row) => Ok(Some(McpToken {
                        created_at: row
                            .context("the catalog returned no row for the token it recorded")?
                            .try_get(0)?,
                        ..token.clone()
                    })),
                    Err(error) if sql_state(&error) == Some(SqlState::UNIQUE_VIOLATION) => Ok(None),
                    Err(error) => Err(error.context("recording an MCP token")),
                }
            }
            #[cfg(test)]
            Self::Memory(tokens) => {
                let mut tokens = tokens.lock().expect("tokens lock");
                if tokens
                    .iter()
                    .any(|(known, _)| known.tenant == token.tenant && known.name == token.name)
                {
                    return Ok(None);
                }
                tokens.insert(0, (token.clone(), digest.to_owned()));
                Ok(Some(token.clone()))
            }
        }
    }

    /// Forget `tenant`'s token named `name`: `false` when there is none.
    async fn remove(&self, tenant: Option<&str>, name: &str) -> Result<bool> {
        match self {
            Self::Pg(db) => Ok(db
                .execute(
                    "DELETE FROM pgokf_web.mcp_tokens
                     WHERE tenant IS NOT DISTINCT FROM $1 AND name = $2",
                    &[&tenant, &name],
                )
                .await
                .context("revoking an MCP token")?
                > 0),
            #[cfg(test)]
            Self::Memory(tokens) => {
                let mut tokens = tokens.lock().expect("tokens lock");
                let before = tokens.len();
                tokens.retain(|(token, _)| {
                    !(token.tenant.as_deref() == tenant && token.name == name)
                });
                Ok(tokens.len() < before)
            }
        }
    }

    /// The digest stored under `name` (in any tenant), for tests of what
    /// minting keeps.
    #[cfg(test)]
    fn digest_under(&self, name: &str) -> Option<String> {
        match self {
            Self::Pg(_) => None,
            Self::Memory(tokens) => tokens
                .lock()
                .expect("tokens lock")
                .iter()
                .find(|(token, _)| token.name == name)
                .map(|(_, digest)| digest.clone()),
        }
    }
}

/// What minting came to.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Minted {
    /// The token, shown once and never stored, beside what was recorded.
    Token { token: String, record: McpToken },
    /// A token of that name exists; revoke it first, or choose another.
    NameTaken,
    /// The name is not one plain word, with why.
    InvalidName(String),
}

/// Minting and revoking, over the store, for the tenant this UI serves.
#[derive(Debug)]
pub(crate) struct McpTokens {
    store: McpTokenStore,
    tenant: Option<String>,
}

impl McpTokens {
    pub(crate) fn new(store: McpTokenStore, tenant: Option<String>) -> Self {
        Self { store, tenant }
    }

    /// The tenant every token minted here is for.
    pub(crate) fn tenant(&self) -> Option<&str> {
        self.tenant.as_deref()
    }

    /// Mint a token for `name` with `role`, recorded as `by`'s and for this
    /// UI's tenant. Only the token's digest is stored. The name is checked
    /// here, before anything is minted, and by the catalog's own constraint
    /// again. What was recorded comes back beside the token, as the catalog
    /// stamped it, for the page that shows it.
    ///
    /// # Errors
    ///
    /// The random source or the catalog failing - never a refusal, which
    /// is a [`Minted`] variant.
    pub(crate) async fn mint(&self, name: &str, role: Role, by: &str) -> Result<Minted> {
        if let Err(why) = valid_token_name(name) {
            return Ok(Minted::InvalidName(why.to_string()));
        }
        let record = McpToken {
            name: name.to_owned(),
            role,
            tenant: self.tenant.clone(),
            created_by: by.to_owned(),
            created_at: crate::documents::now_iso(),
        };
        let token = new_token()?;
        Ok(
            match self.store.insert(&record, &digest_of(&token)).await? {
                Some(record) => Minted::Token { token, record },
                None => Minted::NameTaken,
            },
        )
    }

    /// Every token of this UI's tenant, newest first.
    ///
    /// # Errors
    ///
    /// The catalog cannot be read.
    pub(crate) async fn list(&self) -> Result<Vec<McpToken>> {
        self.store.list(self.tenant()).await
    }

    /// Revoke this UI's tenant's token named `name`: `false` when there is
    /// none. The MCP server refuses it from the next request on.
    ///
    /// # Errors
    ///
    /// The catalog cannot be written.
    pub(crate) async fn revoke(&self, name: &str) -> Result<bool> {
        self.store.remove(self.tenant(), name).await
    }
}

#[cfg(test)]
mod tests {
    use pgokf_companion::mcp_token::{TOKEN_PREFIX, is_minted_token};

    use super::*;
    use crate::db::dead_db;

    fn minted_token(outcome: Minted) -> String {
        match outcome {
            Minted::Token { token, .. } => token,
            other => panic!("expected a token, got {other:?}"),
        }
    }

    fn tokens_for(tenant: Option<&str>) -> McpTokens {
        McpTokens::new(McpTokenStore::memory(), tenant.map(str::to_owned))
    }

    #[tokio::test]
    async fn a_minted_token_is_returned_once_and_kept_only_as_its_digest() {
        // Arrange
        let tokens = tokens_for(None);

        // Act
        let token = minted_token(
            tokens
                .mint("fleet", Role::Builder, "root")
                .await
                .expect("mints"),
        );
        let listed = tokens.list().await.expect("lists");

        // Assert
        assert!(token.starts_with(TOKEN_PREFIX));
        assert!(is_minted_token(&token), "the shape the MCP server admits");
        assert_eq!(
            tokens.store.digest_under("fleet").as_deref(),
            Some(digest_of(&token).as_str())
        );
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].name, "fleet");
        assert_eq!(listed[0].role, Role::Builder);
        assert_eq!(listed[0].created_by, "root");
        assert_eq!(listed[0].tenant, None);
        assert!(
            !format!("{listed:?}").contains(&token),
            "the token is nowhere but in the caller's hands"
        );
    }

    #[tokio::test]
    async fn a_token_is_minted_for_the_tenant_this_ui_serves() {
        // Arrange
        let tokens = tokens_for(Some("acme"));

        // Act
        let minted = tokens
            .mint("fleet", Role::Reader, "root")
            .await
            .expect("mints");

        // Assert
        let Minted::Token { record, .. } = minted else {
            panic!("expected a token, got {minted:?}");
        };
        assert_eq!(record.tenant.as_deref(), Some("acme"));
        assert_eq!(tokens.tenant(), Some("acme"));
        assert_eq!(
            tokens.list().await.expect("lists")[0].tenant.as_deref(),
            Some("acme")
        );
    }

    #[tokio::test]
    async fn a_tenant_sees_and_revokes_its_own_tokens_only() {
        // Arrange: three UIs over one store - one per tenant, and the
        // catalog-wide one.
        let store = McpTokenStore::memory();
        let acme = McpTokens::new(store.shared(), Some("acme".to_owned()));
        let globex = McpTokens::new(store.shared(), Some("globex".to_owned()));
        let open = McpTokens::new(store.shared(), None);
        minted_token(acme.mint("fleet", Role::Reader, "a").await.expect("mints"));
        minted_token(open.mint("fleet", Role::Builder, "o").await.expect("mints"));

        // Act
        let same_name_elsewhere = globex
            .mint("fleet", Role::Reader, "g")
            .await
            .expect("answers");
        let globex_revokes = globex.revoke("fleet").await.expect("answers");
        let acme_sees = acme.list().await.expect("lists");
        let open_sees = open.list().await.expect("lists");
        let globex_sees = globex.list().await.expect("lists");

        // Assert
        assert!(
            matches!(same_name_elsewhere, Minted::Token { .. }),
            "a name is per tenant"
        );
        assert!(globex_revokes, "globex revoked its own fleet");
        assert!(globex_sees.is_empty(), "and only that");
        assert_eq!(acme_sees.len(), 1, "acme's fleet is untouched");
        assert_eq!(acme_sees[0].tenant.as_deref(), Some("acme"));
        assert_eq!(open_sees.len(), 1, "as is the catalog-wide one");
        assert_eq!(open_sees[0].role, Role::Builder);
    }

    #[tokio::test]
    async fn a_name_is_taken_once() {
        // Arrange
        let tokens = tokens_for(None);
        minted_token(
            tokens
                .mint("fleet", Role::Reader, "root")
                .await
                .expect("mints"),
        );

        // Act
        let again = tokens
            .mint("fleet", Role::Builder, "root")
            .await
            .expect("answers");

        // Assert
        assert_eq!(again, Minted::NameTaken);
        let listed = tokens.list().await.expect("lists");
        assert_eq!(listed.len(), 1, "nothing else was recorded");
        assert_eq!(
            listed[0].role,
            Role::Reader,
            "and the first token is untouched"
        );
    }

    #[tokio::test]
    async fn a_bad_name_mints_nothing() {
        // Arrange
        let tokens = tokens_for(None);

        // Act
        let spaced = tokens
            .mint("bad name", Role::Reader, "root")
            .await
            .expect("answers");
        let empty = tokens
            .mint("", Role::Reader, "root")
            .await
            .expect("answers");

        // Assert
        assert!(matches!(spaced, Minted::InvalidName(ref why) if why.contains("bad name")));
        assert!(matches!(empty, Minted::InvalidName(_)));
        assert!(tokens.list().await.expect("lists").is_empty());
    }

    #[tokio::test]
    async fn revoking_forgets_the_token_and_says_whether_there_was_one() {
        // Arrange
        let tokens = tokens_for(None);
        minted_token(
            tokens
                .mint("fleet", Role::Reader, "root")
                .await
                .expect("mints"),
        );

        // Act
        let first = tokens.revoke("fleet").await.expect("revokes");
        let second = tokens.revoke("fleet").await.expect("answers");

        // Assert
        assert!(first);
        assert!(!second);
        assert!(tokens.list().await.expect("lists").is_empty());
        assert_eq!(tokens.store.digest_under("fleet"), None);
    }

    #[tokio::test]
    async fn a_catalog_outage_is_an_error_and_not_a_refusal() {
        // Arrange
        let tokens = McpTokens::new(McpTokenStore::Pg(dead_db()), None);

        // Act
        let mint = tokens.mint("fleet", Role::Reader, "root").await;
        let list = tokens.list().await;
        let revoke = tokens.revoke("fleet").await;

        // Assert
        assert!(mint.is_err(), "not NameTaken, not a token");
        assert!(list.is_err());
        assert!(revoke.is_err(), "never 'there was none'");
    }
}
