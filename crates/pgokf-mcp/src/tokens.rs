// SPDX-License-Identifier: AGPL-3.0-only
//! Who may call this server over HTTP, and which tools they may call.
//!
//! Over stdio the client already holds the connection string and there is
//! nothing to authenticate; over HTTP the server is reachable, so every
//! request carries a bearer token. Tokens are minted on `pgokf-web`'s Admin
//! page (or with `pgokf-web mcp-token mint`), and the catalog keeps only
//! their SHA-256 digests, in `pgokf_web.mcp_tokens` - a table this server's
//! reader role cannot see. It authenticates by hashing the presented token
//! itself and asking `pgokf.mcp_token_bearer(digest)`, a `SECURITY DEFINER`
//! lookup that answers for one digest and lists nothing: the token never
//! travels to the database, and the reader learns nothing about the tokens
//! it does not hold. A revoked token is refused with the very next request;
//! there is no file to re-read and no cache to expire.
//!
//! Only the shape a minted token has is hashed and looked up at all
//! ([`presented_digest`]). A token is a long random string rather than a
//! password - one SHA-256 and an indexed lookup per request, with no slow
//! hash to make a request expensive - and that trade is sound only while the
//! token really is unguessable, so a hand-chosen one is refused rather than
//! silently protected by a fast hash.
//!
//! The lookup has a connection of its own ([`CatalogTokens`]): it is a
//! microsecond index probe, and on the pipelined work connection it would
//! wait behind every tool call in flight, so authentication would queue
//! behind work. A token also names the tenant it was minted for, and the
//! transport admits only tokens minted for the tenant it serves - one
//! process serves one tenant, with tokens of its own.

use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use anyhow::{Context, Result};
pub use pgokf_companion::mcp_token::Role;
use pgokf_companion::mcp_token::{digest_of, is_minted_token};
use tokio::task::JoinHandle;
use tokio_postgres::Client;

/// Bound on one lookup in the catalog: an index probe that takes longer
/// than this is a fault, not work, and the request budget is far too long
/// for one.
const LOOKUP_STATEMENT_TIMEOUT: Duration = Duration::from_secs(5);

/// The bearer of a token: what to call them in the log, what they may do,
/// and the tenant the token was minted for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Bearer {
    pub name: String,
    pub role: Role,
    pub tenant: Option<String>,
}

/// Which tools a role may call: the one place that decides it. `tools/list`
/// filters by the same answer `tools/call` enforces, so a client is never
/// shown a tool its token cannot call.
pub trait ToolAccess: Sized {
    /// Whether this role may call `tool`.
    fn allows_tool(self, tool: &str) -> bool;

    /// Whether any role may call `tool`, which is how a tool this server has
    /// never heard of is told apart from one the caller merely may not
    /// reach.
    fn any_allows(tool: &str) -> bool;
}

impl ToolAccess for Role {
    fn allows_tool(self, tool: &str) -> bool {
        match tool {
            "concept_search" | "find_similar" | "concept_neighbors" | "get_concept"
            | "get_skill" => true,
            "list_plugin_targets" | "build_workspace_plugin" => self >= Role::Builder,
            "list_bundles" | "put_document" | "delete_document" => self >= Role::Writer,
            "create_content_bundle" | "refresh_bundle" | "set_bundle_state" => self >= Role::Admin,
            // A tool no role names is refused here and reported as unknown
            // by the catalog, never silently allowed.
            _ => false,
        }
    }

    fn any_allows(tool: &str) -> bool {
        Self::all().iter().any(|role| role.allows_tool(tool))
    }
}

/// The stored form of a presented token, if it has the shape a catalog
/// mints. Anything else cannot be one, so it is refused before it is hashed.
#[must_use]
pub fn presented_digest(presented: &str) -> Option<String> {
    is_minted_token(presented).then(|| digest_of(presented))
}

/// What the catalog said about a digest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Lookup {
    /// A token this catalog minted.
    Bearer(Bearer),
    /// No token has this digest: wrong, or revoked.
    Unknown,
    /// The catalog knows the token but gives it a role this build does not
    /// know - a newer catalog than this server - which no retry will fix.
    ForeignRole { name: String, role: String },
}

/// A future a trait object can return.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Where tokens are looked up. The catalog answers in production; a test
/// answers whatever the test is about.
pub trait TokenLookups: Send + Sync {
    /// What the catalog knows about `digest`.
    ///
    /// # Errors
    ///
    /// The catalog cannot be asked at all - which is not a refusal.
    fn lookup<'a>(&'a self, digest: &'a str) -> BoxFuture<'a, Result<Lookup>>;

    /// Prove the lookup can be asked: at startup, before a socket is opened
    /// on the strength of it, and again by the health probe.
    ///
    /// # Errors
    ///
    /// The function is missing (a catalog older than 0.2.0), this role may
    /// not execute it, or the connection is gone.
    fn check(&self) -> BoxFuture<'_, Result<()>>;

    /// The lookup connection's driver, to stop with when it ends; `None`
    /// when there is nothing to wait on, or it was already taken.
    fn take_driver(&mut self) -> Option<JoinHandle<()>>;
}

/// The catalog's answer, over a connection of its own (see the module doc).
pub struct CatalogTokens {
    client: Client,
    driver: Option<JoinHandle<()>>,
}

impl CatalogTokens {
    /// Open the lookup connection: the catalog's reader URL, without a
    /// tenant - the lookup reads a table no tenant scopes - and with a short
    /// statement budget of its own.
    ///
    /// # Errors
    ///
    /// The connection cannot be made, or the budget cannot be set.
    pub async fn connect(database_url: &str, force_tls: bool) -> Result<Self> {
        let (client, driver) = pgokf_pgconn::connect(database_url, force_tls)
            .await
            .context("connecting to PostgreSQL for token lookups")?;
        let millis = i32::try_from(LOOKUP_STATEMENT_TIMEOUT.as_millis()).unwrap_or(i32::MAX);
        client
            .execute(
                "SELECT set_config('statement_timeout', $1, false)",
                &[&millis.to_string()],
            )
            .await
            .context("setting the token lookup's statement timeout")?;
        Ok(Self {
            client,
            driver: Some(driver),
        })
    }
}

impl TokenLookups for CatalogTokens {
    fn lookup<'a>(&'a self, digest: &'a str) -> BoxFuture<'a, Result<Lookup>> {
        Box::pin(async move {
            let row = self
                .client
                .query_opt(
                    "SELECT name, role, tenant FROM pgokf.mcp_token_bearer($1)",
                    &[&digest],
                )
                .await
                .context("looking a token up in the catalog")?;
            let Some(row) = row else {
                return Ok(Lookup::Unknown);
            };
            let name: String = row.try_get(0).context("reading the token's name")?;
            let role: String = row.try_get(1).context("reading the token's role")?;
            let tenant: Option<String> = row.try_get(2).context("reading the token's tenant")?;
            Ok(match Role::parse(&role) {
                Some(role) => Lookup::Bearer(Bearer { name, role, tenant }),
                None => Lookup::ForeignRole { name, role },
            })
        })
    }

    fn check(&self) -> BoxFuture<'_, Result<()>> {
        Box::pin(async move {
            let nobody = "0".repeat(64);
            self.client
                .query("SELECT 1 FROM pgokf.mcp_token_bearer($1)", &[&nobody])
                .await
                .context(
                    "the catalog does not answer pgokf.mcp_token_bearer(): the pgokf extension \
                     must be at 0.2.0 or later, and the connection must be a pgokf_reader",
                )?;
            Ok(())
        })
    }

    fn take_driver(&mut self) -> Option<JoinHandle<()>> {
        self.driver.take()
    }
}

#[cfg(test)]
mod tests {
    use pgokf_companion::mcp_token::{TOKEN_PREFIX, new_token};

    use super::*;

    #[test]
    fn a_role_decides_every_tool_and_nothing_else() {
        // Arrange / Act / Assert
        for tool in [
            "concept_search",
            "find_similar",
            "concept_neighbors",
            "get_concept",
            "get_skill",
        ] {
            assert!(Role::Reader.allows_tool(tool), "{tool}");
            assert!(Role::Builder.allows_tool(tool), "{tool}");
        }
        for tool in ["list_plugin_targets", "build_workspace_plugin"] {
            assert!(!Role::Reader.allows_tool(tool), "{tool}");
            assert!(Role::Builder.allows_tool(tool), "{tool}");
        }
        // Writing needs the writer role, and managing bundles the admin's;
        // each holds everything below it.
        for tool in ["list_bundles", "put_document", "delete_document"] {
            assert!(!Role::Builder.allows_tool(tool), "{tool}");
            assert!(Role::Writer.allows_tool(tool), "{tool}");
            assert!(Role::Admin.allows_tool(tool), "{tool}");
        }
        for tool in [
            "create_content_bundle",
            "refresh_bundle",
            "set_bundle_state",
        ] {
            assert!(!Role::Writer.allows_tool(tool), "{tool}");
            assert!(Role::Admin.allows_tool(tool), "{tool}");
        }
        assert!(
            Role::Admin.allows_tool("concept_search") && Role::Writer.allows_tool("get_skill"),
            "a ladder: the higher roles read too"
        );
        assert!(!Role::Admin.allows_tool("drop_everything"));
        assert!(!Role::any_allows("drop_everything"));
        assert!(Role::any_allows("build_workspace_plugin"));
    }

    #[test]
    fn only_a_minted_token_is_even_hashed() {
        // Arrange
        let token = new_token().expect("random");

        // Act / Assert
        let digest = presented_digest(&token).expect("a minted token has a digest");
        assert_eq!(digest, digest_of(&token));
        assert!(
            presented_digest("hunter2").is_none(),
            "a chosen password is not one"
        );
        assert!(
            presented_digest(&token[TOKEN_PREFIX.len()..]).is_none(),
            "no prefix"
        );
        assert!(presented_digest(&format!("{token}x")).is_none(), "too long");
    }
}
