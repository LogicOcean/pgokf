// SPDX-License-Identifier: AGPL-3.0-only
//! Command line and environment configuration for `pgokf-web`.
//!
//! Every setting is available both as a flag and as an `OKF_*` environment
//! variable, matching the other companions, so the compose stack configures
//! the service purely through its environment.

use std::net::SocketAddr;

use anyhow::{Result, bail};
use clap::Parser;

/// `pgokf-web`: the pgokf catalog's web UI and JSON API.
#[derive(Debug, Parser)]
#[command(name = "pgokf-web", version, about)]
pub(crate) struct Cli {
    /// `PostgreSQL` connection string for a `pgokf_reader` role. The UI is
    /// read-only by construction: it never needs a writer or admin role.
    #[arg(long, env = "OKF_PG_URL", hide_env_values = true)]
    pub database_url: String,

    /// Socket address to listen on.
    #[arg(long, env = "OKF_WEB_BIND", default_value = "127.0.0.1:8080")]
    pub bind: SocketAddr,

    /// Optional multi-tenant scope applied as `pgokf.tenant` on every pooled
    /// connection (required once the catalog's `require_tenant` policy is on).
    #[arg(long, env = "OKF_TENANT")]
    pub tenant: Option<String>,

    /// Force TLS to `PostgreSQL` even when the URL does not require it.
    #[arg(long, env = "OKF_PG_TLS", default_value_t = false)]
    pub tls: bool,

    /// Maximum pooled `PostgreSQL` connections.
    #[arg(long, env = "OKF_WEB_POOL_SIZE", default_value_t = 8)]
    pub pool_size: usize,

    /// Statement timeout applied to every pooled connection, in milliseconds.
    /// Bounds the cost of any single page or API request.
    #[arg(long, env = "OKF_WEB_STATEMENT_TIMEOUT_MS", default_value_t = 15_000)]
    pub statement_timeout_ms: u64,

    /// Base URL of an OpenAI-compatible embeddings server (without
    /// `/v1/embeddings`). When set together with `--embed-model`, the search
    /// page offers semantic and hybrid modes by embedding the query here.
    #[arg(long, env = "OKF_EMBED_ENDPOINT")]
    pub embed_endpoint: Option<String>,

    /// Embedding model name for the endpoint above; must produce vectors of
    /// the catalog's `embedding_dim`.
    #[arg(long, env = "OKF_EMBED_MODEL")]
    pub embed_model: Option<String>,

    /// Bearer token for the embeddings endpoint, when it needs one.
    #[arg(long, env = "OKF_EMBED_API_KEY", hide_env_values = true)]
    pub embed_api_key: Option<String>,

    /// Display name for this catalog in the page header (defaults to the
    /// database name from the connection string).
    #[arg(long, env = "OKF_WEB_TITLE")]
    pub title: Option<String>,
}

impl Cli {
    /// Treat empty optional values as unset (the shape a compose stack
    /// produces for an unset variable), mirroring the other companions.
    pub(crate) fn normalized(mut self) -> Self {
        self.tenant = pgokf_companion::cli::non_empty(self.tenant);
        self.embed_endpoint = pgokf_companion::cli::non_empty(self.embed_endpoint);
        self.embed_model = pgokf_companion::cli::non_empty(self.embed_model);
        self.embed_api_key = pgokf_companion::cli::non_empty(self.embed_api_key);
        self.title = pgokf_companion::cli::non_empty(self.title);
        self
    }

    /// Reject contradictory settings before anything connects.
    pub(crate) fn validate(&self) -> Result<()> {
        if self.pool_size == 0 {
            bail!("--pool-size must be at least 1");
        }
        if self.statement_timeout_ms == 0 {
            bail!("--statement-timeout-ms must be at least 1");
        }
        if self.embed_endpoint.is_some() != self.embed_model.is_some() {
            bail!("--embed-endpoint and --embed-model must be given together");
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(extra: &[&str]) -> Cli {
        let mut args = vec![
            "pgokf-web",
            "--database-url",
            "postgresql://okf_reader@localhost/okf",
        ];
        args.extend_from_slice(extra);
        Cli::parse_from(args)
    }

    #[test]
    fn normalized_treats_empty_optional_values_as_unset() {
        // Arrange: the shape a compose stack produces for unset variables.
        let cli = parse(&["--tenant", "", "--embed-endpoint", "", "--embed-model", ""]);

        // Act
        let cli = cli.normalized();

        // Assert
        assert_eq!(cli.tenant, None);
        assert_eq!(cli.embed_endpoint, None);
        assert_eq!(cli.embed_model, None);
    }

    #[test]
    fn validate_requires_endpoint_and_model_together() {
        // Arrange
        let cli = parse(&["--embed-endpoint", "http://embed:8080"]).normalized();

        // Act
        let result = cli.validate();

        // Assert
        assert!(result.is_err());
    }

    #[test]
    fn validate_accepts_the_defaults() {
        // Arrange
        let cli = parse(&[]).normalized();

        // Act & Assert
        assert!(cli.validate().is_ok());
        assert_eq!(cli.bind.port(), 8080);
    }
}
