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

    /// `PostgreSQL` connection string for a `pgokf_writer` role, used by the
    /// human workflow (upload, edit, review) for people whose role allows
    /// it, and - through a pool of its own - by the `users` and `oidc`
    /// modes for the people and sessions they keep in the catalog and by
    /// the Admin page (and `pgokf-web mcp-token`) for the MCP tokens it
    /// mints. Without it those pages and modes are off.
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

    /// How people are identified: `none` (everyone is a viewer), `oidc`
    /// (this site signs people in against an `OpenID` Connect provider),
    /// `header` (a trusted reverse proxy forwards the identity in headers),
    /// or `users` (people kept in the catalog, with a login form).
    #[arg(long = "auth", env = "OKF_WEB_AUTH", default_value = "none")]
    pub auth: String,

    /// `oidc` mode: the provider's issuer URL, exactly as it declares it
    /// (its configuration is read from
    /// `<issuer>/.well-known/openid-configuration`).
    #[arg(long, env = "OKF_WEB_OIDC_ISSUER")]
    pub oidc_issuer: Option<String>,

    /// `oidc` mode: the client id this site is registered with.
    #[arg(long, env = "OKF_WEB_OIDC_CLIENT_ID")]
    pub oidc_client_id: Option<String>,

    /// `oidc` mode: the client secret, for a confidential client. Leave it
    /// unset for a public client, which PKCE alone protects.
    #[arg(long, env = "OKF_WEB_OIDC_CLIENT_SECRET", hide_env_values = true)]
    pub oidc_client_secret: Option<String>,

    /// `oidc` mode: this site's callback URL, registered with the provider
    /// as a redirect URI. It is the site's public address plus
    /// `/auth/callback`.
    #[arg(long, env = "OKF_WEB_OIDC_REDIRECT_URL")]
    pub oidc_redirect_url: Option<String>,

    /// `oidc` mode: the scopes to ask for (`openid` is always included).
    #[arg(
        long,
        env = "OKF_WEB_OIDC_SCOPES",
        default_value = "openid profile email"
    )]
    pub oidc_scopes: String,

    /// `oidc` mode: the claims tried in order for the person's identity,
    /// which becomes their OKF actor (`human:<subject>`). Name a claim the
    /// provider guarantees stable and unique: `sub` always is, while a
    /// user name or an email can be reassigned to someone else.
    #[arg(
        long,
        env = "OKF_WEB_OIDC_SUBJECT_CLAIMS",
        default_value = "preferred_username,email,sub"
    )]
    pub oidc_subject_claims: String,

    /// `oidc` mode: the claim carrying the person's groups, which
    /// `--auth-role-map` turns into a role.
    #[arg(long, env = "OKF_WEB_OIDC_GROUPS_CLAIM", default_value = "groups")]
    pub oidc_groups_claim: String,

    /// `oidc` mode: what the sign-in button calls the provider.
    #[arg(
        long,
        env = "OKF_WEB_OIDC_PROVIDER_NAME",
        default_value = "single sign-on"
    )]
    pub oidc_provider_name: String,

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

    /// Base URL of the repository-registry producer service's admin API
    /// (without `/admin/...`). Together with the `OKF_PRODUCER_ADMIN_TOKEN`
    /// environment variable it enables the Admin page's Registry tab
    /// credential controls; the tab's registry table itself is read from
    /// the catalog database either way.
    #[arg(long, env = "OKF_PRODUCER_ADMIN_URL")]
    pub producer_admin_url: Option<String>,

    /// The static admin bearer token the producer's admin API requires.
    /// Settable only through the `OKF_PRODUCER_ADMIN_TOKEN` environment
    /// variable (read after parsing) - never a CLI flag, whose value would
    /// show in the process listing. Held in this process's memory only:
    /// never written to the database, never logged, never rendered into a
    /// page.
    #[arg(skip)]
    pub producer_admin_token: Option<String>,

    /// Display name for this catalog in the page header (defaults to the
    /// database name from the connection string).
    #[arg(long, env = "OKF_WEB_TITLE")]
    pub title: Option<String>,
}

/// Maintenance commands that run without a catalog.
#[derive(Debug, Subcommand)]
pub(crate) enum Command {
    /// Manage the people the `users` identity mode signs in. They live in
    /// the catalog (`pgokf_web.users`), so these need `--writer-url`.
    #[command(subcommand)]
    User(UserCommand),
    /// Manage the bearer tokens that may call `pgokf-mcp` over HTTP. They
    /// live in the catalog (`pgokf_web.mcp_tokens`), so these need
    /// `--writer-url`; the Admin page does the same.
    #[command(subcommand)]
    McpToken(McpTokenCommand),
}

#[derive(Debug, Subcommand)]
pub(crate) enum McpTokenCommand {
    /// Mint a token for this process's `--tenant` (or none) and print it
    /// once, alone on standard output; only its digest is stored, so
    /// nothing can recover it later.
    Mint {
        /// What to call the token in the MCP server's log (letters,
        /// digits, . _ - @ +).
        #[arg(long)]
        name: String,
        /// What it may do: reader (search and read the catalog) or builder
        /// (also build workspace plugins).
        #[arg(long, default_value = "reader")]
        role: String,
    },
    /// List the tokens, one per line: name, role, tenant (`-` for none), who
    /// minted it, and when - tab-separated, without a header, for scripts.
    List,
    /// Revoke a token; the MCP server refuses it from the next request on.
    Revoke {
        /// The token's name.
        #[arg(long)]
        name: String,
    },
}

#[derive(Debug, Subcommand)]
pub(crate) enum UserCommand {
    /// Add a person, with the password read from standard input. This is
    /// how the first admin is made; the Admin page does the rest.
    Add {
        /// The sign-in name (letters, digits, . _ - @ +).
        #[arg(long)]
        name: String,
        /// The role: viewer, uploader, editor, approver, or admin.
        #[arg(long, default_value = "viewer")]
        role: String,
        /// What to call the person, when that is more than their sign-in
        /// name (shown wherever they are; their OKF actor stays the name).
        #[arg(long)]
        display: Option<String>,
    },
    /// Replace a person's password with one read from standard input, and
    /// end every session they hold - the way back in when an admin is
    /// locked out.
    SetPassword {
        /// The sign-in name.
        #[arg(long)]
        name: String,
    },
}

impl Cli {
    /// Treat empty optional values as unset (the shape a compose stack
    /// produces for an unset variable), mirroring the other companions.
    pub(crate) fn normalized(self) -> Self {
        self.normalized_with_token_env(std::env::var("OKF_PRODUCER_ADMIN_TOKEN").ok())
    }

    /// The env read separated from the normalization, so tests exercise it
    /// without touching the process environment. The producer admin token
    /// is environment-only by design (a CLI flag would leak the secret
    /// through the process listing), so clap never fills it.
    fn normalized_with_token_env(mut self, token_env: Option<String>) -> Self {
        if self.producer_admin_token.is_none() {
            self.producer_admin_token = token_env;
        }
        self.writer_url = pgokf_companion::cli::non_empty(self.writer_url);
        self.auth_name_header = pgokf_companion::cli::non_empty(self.auth_name_header);
        self.auth_groups_header = pgokf_companion::cli::non_empty(self.auth_groups_header);
        self.session_secret = pgokf_companion::cli::non_empty(self.session_secret);
        self.oidc_issuer = pgokf_companion::cli::non_empty(self.oidc_issuer);
        self.oidc_client_id = pgokf_companion::cli::non_empty(self.oidc_client_id);
        self.oidc_client_secret = pgokf_companion::cli::non_empty(self.oidc_client_secret);
        self.oidc_redirect_url = pgokf_companion::cli::non_empty(self.oidc_redirect_url);
        self.bundles_dir = self.bundles_dir.filter(|p| !p.as_os_str().is_empty());
        self.bundles_db_dir = pgokf_companion::cli::non_empty(self.bundles_db_dir);
        self.tenant = pgokf_companion::cli::non_empty(self.tenant);
        self.embed_endpoint = pgokf_companion::cli::non_empty(self.embed_endpoint);
        self.embed_model = pgokf_companion::cli::non_empty(self.embed_model);
        self.embed_api_key = pgokf_companion::cli::non_empty(self.embed_api_key);
        self.producer_admin_url = pgokf_companion::cli::non_empty(self.producer_admin_url);
        self.producer_admin_token = pgokf_companion::cli::non_empty(self.producer_admin_token);
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
        if self.producer_admin_url.is_some() != self.producer_admin_token.is_some() {
            bail!("--producer-admin-url and --producer-admin-token must be given together");
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
                if self.writer_url.is_none() {
                    bail!(
                        "--auth users needs --writer-url: the people and the sessions live in \
                         the catalog (pgokf_web.users / pgokf_web.sessions), reached through \
                         the writer connection"
                    );
                }
                if self.session_hours == 0 {
                    bail!("--session-hours must be at least 1");
                }
            }
            "oidc" => {
                for (value, flag) in [
                    (&self.oidc_issuer, "--oidc-issuer"),
                    (&self.oidc_client_id, "--oidc-client-id"),
                    (&self.oidc_redirect_url, "--oidc-redirect-url"),
                ] {
                    if value.is_none() {
                        bail!("--auth oidc needs {flag}");
                    }
                }
                if self.session_hours == 0 {
                    bail!("--session-hours must be at least 1");
                }
                if self.writer_url.is_none() {
                    bail!(
                        "--auth oidc needs --writer-url: sessions live in the catalog \
                         (pgokf_web.sessions), reached through the writer connection, so \
                         signing out ends a session and an admin can end someone's"
                    );
                }
            }
            other => bail!("--auth must be none, oidc, header, or users (not {other:?})"),
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
        // People and sessions live in the catalog, so the modes that keep
        // them need the writer connection.
        assert!(parse(&["--auth", "users"]).validate().is_err());
        let writer = "postgresql://okf_writer@localhost/okf";
        assert!(
            parse(&["--auth", "users", "--writer-url", writer])
                .validate()
                .is_ok()
        );
        assert!(parse(&["--auth", "oidc"]).validate().is_err());
        let oidc = [
            "--auth",
            "oidc",
            "--oidc-issuer",
            "https://id.example.test",
            "--oidc-client-id",
            "pgokf",
            "--oidc-redirect-url",
            "https://catalog.example.test/auth/callback",
        ];
        assert!(
            parse(&oidc).validate().is_err(),
            "oidc keeps its sessions in the catalog, so it needs the writer too"
        );
        let mut with_writer = oidc.to_vec();
        with_writer.extend(["--writer-url", writer]);
        assert!(parse(&with_writer).validate().is_ok());
        let sub = Cli::parse_from([
            "pgokf-web",
            "user",
            "add",
            "--name",
            "alice",
            "--role",
            "editor",
        ]);
        assert!(matches!(
            sub.command,
            Some(Command::User(UserCommand::Add { .. }))
        ));
        assert!(
            sub.database_url.is_none(),
            "a user command parses without the reader URL (it uses the writer at run time)"
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

    #[test]
    fn the_producer_admin_settings_come_in_a_pair() {
        // Arrange: the token is environment-only - a CLI flag would leak it
        // through the process listing - so there is no flag to parse.

        // Act & Assert: the flag does not exist...
        let unknown = Cli::try_parse_from([
            "pgokf-web",
            "--database-url",
            "postgresql://okf_reader@localhost/okf",
            "--producer-admin-token",
            "token",
        ])
        .expect_err("no --producer-admin-token flag exists");
        assert_eq!(unknown.kind(), clap::error::ErrorKind::UnknownArgument);
        // ...the URL alone is half a pair...
        assert!(
            parse(&["--producer-admin-url", "http://producer:8081"])
                .normalized_with_token_env(None)
                .validate()
                .is_err()
        );
        // ...and the token alone (through the environment) is the other
        // half.
        let cli = parse(&[]).normalized_with_token_env(Some("token".to_owned()));
        assert_eq!(cli.producer_admin_token.as_deref(), Some("token"));
        assert!(cli.validate().is_err(), "the token alone is half a pair");
        let cli = parse(&["--producer-admin-url", "http://producer:8081"])
            .normalized_with_token_env(Some("token".to_owned()));
        assert!(cli.validate().is_ok(), "the pair validates together");

        // An empty value is the compose shape of "unset", so half a pair
        // that way is simply no producer configuration at all.
        let cli =
            parse(&["--producer-admin-url", ""]).normalized_with_token_env(Some(String::new()));
        assert_eq!(cli.producer_admin_url, None);
        assert_eq!(cli.producer_admin_token, None);
        assert!(cli.validate().is_ok());
    }
}
