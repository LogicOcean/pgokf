// SPDX-License-Identifier: AGPL-3.0-only
//! Command line and environment configuration for `pgokf-web`.
//!
//! Every setting is available both as a flag and as an `OKF_*` environment
//! variable, matching the other companions, so the compose stack configures
//! the service purely through its environment.

use std::net::SocketAddr;
use std::path::PathBuf;

use anyhow::{Result, bail};
use clap::{Parser, Subcommand};

/// `pgokf-web`: the pgokf catalog's web UI and JSON API.
#[derive(Debug, Parser)]
#[command(name = "pgokf-web", version, about)]
pub(crate) struct Cli {
    #[command(subcommand)]
    pub command: Option<Command>,

    /// `PostgreSQL` connection string for a `pgokf_reader` role: everything
    /// the UI shows comes through it.
    #[arg(long, env = "OKF_PG_URL", hide_env_values = true)]
    pub database_url: Option<String>,

    /// `PostgreSQL` connection string for a `pgokf_writer` role, used only by
    /// the human workflow (upload, edit, review) and only for people whose
    /// role allows it. Without it those pages are off.
    #[arg(long, env = "OKF_PG_WRITER_URL", hide_env_values = true)]
    pub writer_url: Option<String>,

    /// The directory under which directory bundles are reachable from this
    /// process, mounted read-write: editors can then change their documents
    /// in place (the bundle is refreshed afterwards). Unset, such bundles are
    /// read-only in the UI.
    #[arg(long, env = "OKF_WEB_BUNDLES_DIR")]
    pub bundles_dir: Option<PathBuf>,

    /// The path the database server uses for that same directory, when it
    /// differs from `--bundles-dir` (the prefix of the registered bundle
    /// paths). Defaults to `--bundles-dir`.
    #[arg(long, env = "OKF_WEB_BUNDLES_DB_DIR")]
    pub bundles_db_dir: Option<String>,

    /// How people are identified: `none` (everyone is a viewer), `header`
    /// (a trusted reverse proxy forwards the identity in headers), or
    /// `users` (a local users file with a login form).
    #[arg(long = "auth", env = "OKF_WEB_AUTH", default_value = "none")]
    pub auth: String,

    /// `header` mode: the header carrying the user's identifier.
    #[arg(
        long,
        env = "OKF_WEB_AUTH_USER_HEADER",
        default_value = "X-Forwarded-User"
    )]
    pub auth_user_header: String,

    /// `header` mode: an optional header carrying a display name.
    #[arg(long, env = "OKF_WEB_AUTH_NAME_HEADER")]
    pub auth_name_header: Option<String>,

    /// `header` mode: an optional header carrying comma-separated groups.
    #[arg(
        long,
        env = "OKF_WEB_AUTH_GROUPS_HEADER",
        default_value = "X-Forwarded-Groups"
    )]
    pub auth_groups_header: Option<String>,

    /// `header` mode: `group=role,...` mapping groups to roles (viewer,
    /// uploader, editor, approver, admin); the highest matching role wins.
    #[arg(long, env = "OKF_WEB_AUTH_ROLE_MAP", default_value = "")]
    pub auth_role_map: String,

    /// `header` mode: the role of an identified person in no mapped group.
    #[arg(long, env = "OKF_WEB_AUTH_DEFAULT_ROLE", default_value = "viewer")]
    pub auth_default_role: String,

    /// `header` mode: comma-separated addresses or CIDR ranges the proxy
    /// connects from; identity headers from anywhere else are ignored. The
    /// word `any` believes every peer (only on a network where nothing but
    /// the proxy can reach this server).
    #[arg(long, env = "OKF_WEB_AUTH_TRUSTED_PROXY", default_value = "")]
    pub auth_trusted_proxy: String,

    /// `users` mode: the users file (`name:role:$argon2id$...` per line;
    /// `pgokf-web hash-password` produces a line).
    #[arg(long, env = "OKF_WEB_AUTH_USERS_FILE")]
    pub auth_users_file: Option<PathBuf>,

    /// `users` mode: the key that signs session cookies (at least 32
    /// characters). Unset, a random key is used and sessions end with the
    /// process.
    #[arg(long, env = "OKF_WEB_SESSION_SECRET", hide_env_values = true)]
    pub session_secret: Option<String>,

    /// `users` mode: how long a session lasts, in hours.
    #[arg(long, env = "OKF_WEB_SESSION_HOURS", default_value_t = 12)]
    pub session_hours: u64,

    /// `users` mode: mark the session cookie `Secure` (the site is served
    /// over HTTPS).
    #[arg(long, env = "OKF_WEB_COOKIE_SECURE", default_value_t = false)]
    pub cookie_secure: bool,

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

/// Maintenance commands that run without a catalog.
#[derive(Debug, Subcommand)]
pub(crate) enum Command {
    /// Hash a password read from standard input and print a users-file line
    /// (`name:role:$argon2id$...`) for it.
    HashPassword {
        /// The user name the line is for.
        #[arg(long)]
        user: String,
        /// The user's role: viewer, uploader, editor, approver, or admin.
        #[arg(long, default_value = "viewer")]
        role: String,
    },
}

impl Cli {
    /// Treat empty optional values as unset (the shape a compose stack
    /// produces for an unset variable), mirroring the other companions.
    pub(crate) fn normalized(mut self) -> Self {
        self.writer_url = pgokf_companion::cli::non_empty(self.writer_url);
        self.auth_name_header = pgokf_companion::cli::non_empty(self.auth_name_header);
        self.auth_groups_header = pgokf_companion::cli::non_empty(self.auth_groups_header);
        self.session_secret = pgokf_companion::cli::non_empty(self.session_secret);
        self.auth_users_file = self.auth_users_file.filter(|p| !p.as_os_str().is_empty());
        self.bundles_dir = self.bundles_dir.filter(|p| !p.as_os_str().is_empty());
        self.bundles_db_dir = pgokf_companion::cli::non_empty(self.bundles_db_dir);
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
        match self.auth.trim() {
            "none" => {}
            "header" => {
                if self.auth_trusted_proxy.trim().is_empty() {
                    bail!(
                        "--auth header needs --auth-trusted-proxy (the proxy's address or range, \
                         or the word any)"
                    );
                }
            }
            "users" => {
                if self.auth_users_file.is_none() {
                    bail!("--auth users needs --auth-users-file");
                }
                if self.session_hours == 0 {
                    bail!("--session-hours must be at least 1");
                }
            }
            other => bail!("--auth must be none, header, or users (not {other:?})"),
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
    fn auth_modes_demand_their_own_settings() {
        // Arrange / Act / Assert
        assert!(parse(&["--auth", "header"]).validate().is_err());
        assert!(
            parse(&["--auth", "header", "--auth-trusted-proxy", "10.0.0.0/8"])
                .validate()
                .is_ok()
        );
        assert!(parse(&["--auth", "users"]).validate().is_err());
        assert!(
            parse(&["--auth", "users", "--auth-users-file", "/tmp/users"])
                .validate()
                .is_ok()
        );
        assert!(parse(&["--auth", "oidc"]).validate().is_err());
        let sub = Cli::parse_from([
            "pgokf-web",
            "hash-password",
            "--user",
            "alice",
            "--role",
            "editor",
        ]);
        assert!(matches!(sub.command, Some(Command::HashPassword { .. })));
        assert!(
            sub.database_url.is_none(),
            "a maintenance command needs no catalog"
        );
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
