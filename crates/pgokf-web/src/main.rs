// SPDX-License-Identifier: AGPL-3.0-only
//! `pgokf-web`: the pgokf catalog's web UI and JSON API.
//!
//! A thin companion: it renders what the `pgokf_reader` role can see
//! through the public SQL API (search, browse, inspect, monitor) and never
//! holds catalogue semantics of its own. With a writer connection and an
//! authentication mode it also carries the human workflow (upload, edit,
//! review), which writes ordinary OKF documents into content bundles.
//! Configuration is by flags or `OKF_*` environment variables like the
//! other companions; the companions image ships the binary.

mod auth;
mod config;
mod db;
mod documents;
mod graph;
mod links;
mod markdown;
mod mcp_tokens;
mod oidc;
mod oidc_settings;
mod provider;
mod routes;
mod seal;
mod session_store;
mod store;
mod user_store;

use std::io::Read;
use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use axum::http::HeaderName;
use clap::Parser;
use pgokf_companion::embeddings::EmbeddingsClient;
use pgokf_companion::mcp_token::Role as McpRole;

use crate::auth::{Authenticator, Cidr, HeaderAuth, Role, RoleMapping, Sessions, UsersAuth};
use crate::config::{Cli, Command, McpTokenCommand, UserCommand};
use crate::db::{Db, DbConfig};
use crate::mcp_tokens::{McpTokenStore, McpTokens, Minted};
use crate::oidc_settings::OidcSettingsStore;
use crate::provider::ProviderSlot;
use crate::routes::App;
use crate::seal::Sealer;
use crate::session_store::SessionStore;
use crate::user_store::UserStore;

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse().normalized();
    if let Some(command) = &cli.command {
        return run_command(&cli, command).await;
    }
    cli.validate()?;
    let database_url = cli
        .database_url
        .clone()
        .context("--database-url (OKF_PG_URL) is required")?;

    let db = Db::connect(&DbConfig {
        database_url: &database_url,
        force_tls: cli.tls,
        pool_size: cli.pool_size,
        tenant: cli.tenant.as_deref(),
        statement_timeout_ms: cli.statement_timeout_ms,
    })?;
    let writer = connect_writer(&cli).await?;
    let identity = connect_identity(&cli).await?;
    let authenticator = build_authenticator(&cli, identity.as_ref())?;
    let stores = configured_stores(&cli)?;
    // Fail fast on a bad connection string or role: the first page would
    // otherwise be the first error.
    let (version, sql_version) = db
        .versions()
        .await
        .context("the catalog is not reachable with the configured reader connection")?;

    let embedder = match (&cli.embed_endpoint, &cli.embed_model) {
        (Some(endpoint), Some(model)) => {
            let client = EmbeddingsClient::new(endpoint, model.clone(), cli.embed_api_key.clone())?;
            check_embedding_dimension(&db, &client, model).await?;
            Some(client)
        }
        _ => None,
    };
    let catalog_name = cli.title.clone().unwrap_or_else(|| {
        pgokf_pgconn::parse_config(&database_url)
            .ok()
            .and_then(|c| c.get_dbname().map(str::to_owned))
            .unwrap_or_else(|| "pgokf".to_owned())
    });

    let trusted_proxies = auth::TrustedProxies::parse(&cli.auth_trusted_proxy)
        .context("parsing --auth-trusted-proxy")?;

    // MCP tokens are an admin's to mint, over the identity pool: small, and
    // never behind the workflow's minute-long resyncs.
    let mcp_tokens = identity
        .clone()
        .map(|identity| McpTokens::new(McpTokenStore::Pg(identity), cli.tenant.clone()));
    let app = Arc::new(App {
        db,
        writer,
        mcp_tokens,
        auth: authenticator,
        trusted_proxies,
        rebuilds: tokio::sync::Mutex::new(()),
        builds: tokio::sync::Semaphore::new(routes::MAX_PLUGIN_BUILDS),
        stores,
        embedder,
        catalog_name,
        tenant: cli.tenant.clone(),
        version: version.clone(),
    });
    let router = routes::router(app.clone());

    let listener = tokio::net::TcpListener::bind(cli.bind)
        .await
        .with_context(|| format!("binding {}", cli.bind))?;
    eprintln!(
        "pgokf-web: serving catalog {} (pgokf {version}, SQL {sql_version}) on http://{}{}; auth {}{}",
        cli.title.as_deref().unwrap_or("(from connection)"),
        cli.bind,
        cli.tenant
            .as_deref()
            .map(|t| format!(" as tenant {t}"))
            .unwrap_or_default(),
        app.auth.mode().id(),
        if cli.writer_url.is_some() {
            ", human workflow on"
        } else {
            ", read-only"
        }
    );
    let shutdown = pgokf_companion::daemon::shutdown_signal()?;
    // Connection info gives the auth seam the peer address, which is what
    // decides whether a proxy's identity headers are believed.
    axum::serve(
        listener,
        router.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(async move {
        if let Err(error) = shutdown.await {
            eprintln!("pgokf-web: shutdown signal error: {error}");
        }
    })
    .await
    .context("serving HTTP")?;
    eprintln!("pgokf-web: stopped");
    Ok(())
}

/// The writer pool: small, and used only by the human workflow.
async fn connect_writer(cli: &Cli) -> Result<Option<Db>> {
    let Some(url) = &cli.writer_url else {
        return Ok(None);
    };
    let writer = Db::connect(&DbConfig {
        database_url: url,
        force_tls: cli.tls,
        pool_size: 2,
        tenant: cli.tenant.as_deref(),
        statement_timeout_ms: cli.statement_timeout_ms.max(60_000),
    })?;
    writer
        .versions()
        .await
        .context("the catalog is not reachable with the configured writer connection")?;
    Ok(Some(writer))
}

/// Connections for identity lookups (`pgokf_web.users` / `pgokf_web.sessions`):
/// the writer URL, but a pool of its own, never shared with the human
/// workflow's minute-long resyncs, so a session check can never queue behind
/// an upload - and with a short statement budget, since a lookup that takes
/// longer than this is a fault, not work.
const IDENTITY_POOL: usize = 4;
const IDENTITY_STATEMENT_MS: u64 = 5_000;

/// The `pgokf_web` tables the identity modes keep people and sessions in,
/// and the one every writer-backed deployment keeps its MCP tokens in.
const IDENTITY_TABLES: &[&str] = &["pgokf_web.users", "pgokf_web.sessions", "pgokf_web.oidc"];
const MCP_TOKEN_TABLES: &[&str] = &["pgokf_web.mcp_tokens"];

/// The identity pool, whenever there is a writer URL: the `users` and
/// `oidc` modes keep people and sessions in it, every mode with a writer
/// keeps the MCP tokens the Admin page mints in it, and the maintenance
/// commands need nothing bigger. Probed at startup, so a writer URL that
/// cannot see `pgokf_web` (a reader role, a catalog older than 0.2.0) fails
/// here, with the reason, rather than at the first person's sign-in or on
/// the Admin page.
async fn connect_identity(cli: &Cli) -> Result<Option<Db>> {
    let Some(url) = cli.writer_url.as_deref() else {
        return Ok(None);
    };
    let identity = identity_pool(cli, url)?;
    probe_tables(&identity, MCP_TOKEN_TABLES).await?;
    let mode = cli.auth.trim();
    if !matches!(mode, "users" | "oidc") {
        return Ok(Some(identity));
    }
    probe_tables(&identity, IDENTITY_TABLES).await?;
    if mode == "users" {
        let people = identity
            .query_one("SELECT count(*) FROM pgokf_web.users", &[])
            .await
            .and_then(|row| row.try_get::<_, i64>(0).context("reading the count"))?;
        if people == 0 {
            eprintln!(
                "pgokf-web: warning: nobody can sign in yet - add the first admin with \
                 `pgokf-web user add --name NAME --role admin < password.txt`"
            );
        }
    }
    Ok(Some(identity))
}

fn identity_pool(cli: &Cli, url: &str) -> Result<Db> {
    Db::connect(&DbConfig {
        database_url: url,
        force_tls: cli.tls,
        pool_size: IDENTITY_POOL,
        tenant: cli.tenant.as_deref(),
        statement_timeout_ms: IDENTITY_STATEMENT_MS,
    })
}

/// Fail fast when `tables` cannot be read as configured.
async fn probe_tables(db: &Db, tables: &[&str]) -> Result<()> {
    for table in tables {
        db.query(&format!("SELECT 1 FROM {table} LIMIT 0"), &[])
            .await
            .with_context(|| {
                format!(
                    "the writer connection cannot read {table}: --writer-url must be the \
                     pgokf_writer role and the catalog must be at 0.2.0 or later"
                )
            })?;
    }
    Ok(())
}

/// Where directory bundles are reachable, from the flags.
fn configured_stores(cli: &Cli) -> Result<store::Stores> {
    let Some(dir) = &cli.bundles_dir else {
        return Ok(store::Stores::default());
    };
    let local_root = dir
        .canonicalize()
        .with_context(|| format!("--bundles-dir {} is not a directory", dir.display()))?;
    Ok(store::Stores {
        db_root: Some(
            cli.bundles_db_dir
                .clone()
                .unwrap_or_else(|| dir.to_string_lossy().into_owned()),
        ),
        local_root: Some(local_root),
    })
}

/// The session signer for the modes that hold a session, remembering every
/// session it issues in the catalog (`pgokf_web.sessions`) through
/// `identity`, so a session can be ended rather than merely left to expire.
/// Without a configured secret a random one is used, which means the
/// sessions end when the process does. `serving` says whether to say so:
/// a maintenance command mints no cookie and need not warn.
fn build_sessions(cli: &Cli, identity: Db, serving: bool) -> Result<Arc<Sessions>> {
    let secret = if let Some(secret) = &cli.session_secret {
        secret.as_bytes().to_vec()
    } else {
        if serving {
            eprintln!(
                "pgokf-web: warning: no OKF_WEB_SESSION_SECRET; sessions end when the \
                 process does"
            );
        }
        auth::random_bytes(32)?
    };
    let sessions = Sessions::new(
        secret,
        cli.session_hours.saturating_mul(3_600),
        cli.cookie_secure,
    )?;
    if serving {
        eprintln!(
            "pgokf-web: live sessions are recorded in the catalog (pgokf_web.sessions), so \
             signing out ends a session everywhere"
        );
    }
    Ok(Arc::new(sessions.with_store(SessionStore::Pg(identity))))
}

/// The identity mode, from the flags. The two that keep state - `users`
/// (people) and `oidc` (sessions) - keep it in the catalog through the
/// identity pool, which [`connect_identity`] has already opened and probed.
fn build_authenticator(cli: &Cli, identity: Option<&Db>) -> Result<Authenticator> {
    let identity_for = |mode: &str| {
        identity.cloned().with_context(|| {
            format!("--auth {mode} needs --writer-url: its people and sessions live in the catalog")
        })
    };
    match cli.auth.trim() {
        "oidc" => {
            let required = |value: &Option<String>, flag: &str| -> Result<String> {
                value
                    .clone()
                    .with_context(|| format!("--auth oidc needs {flag}"))
            };
            let default_role = Role::parse(&cli.auth_default_role)
                .with_context(|| format!("unknown default role {:?}", cli.auth_default_role))?;
            let subject_claims: Vec<String> = cli
                .oidc_subject_claims
                .split(',')
                .map(str::trim)
                .filter(|c| !c.is_empty())
                .map(str::to_owned)
                .collect();
            let config = oidc::OidcConfig {
                issuer: required(&cli.oidc_issuer, "--oidc-issuer")?,
                client_id: required(&cli.oidc_client_id, "--oidc-client-id")?,
                client_secret: cli.oidc_client_secret.clone(),
                redirect_uri: required(&cli.oidc_redirect_url, "--oidc-redirect-url")?,
                scopes: cli.oidc_scopes.clone(),
                subject_claims,
                groups_claim: cli.oidc_groups_claim.trim().to_owned(),
                roles: RoleMapping::parse(&cli.auth_role_map, default_role)?,
                provider_name: cli.oidc_provider_name.trim().to_owned(),
            };
            Ok(Authenticator::Oidc(Arc::new(oidc::OidcAuth::new(
                config,
                build_sessions(cli, identity_for("oidc")?, true)?,
            )?)))
        }
        "header" => {
            let header = |name: &str| {
                HeaderName::from_bytes(name.trim().as_bytes())
                    .with_context(|| format!("{name:?} is not a valid header name"))
            };
            let trusted_text = cli.auth_trusted_proxy.trim();
            let trust_any_peer = trusted_text.eq_ignore_ascii_case("any");
            let trusted = if trust_any_peer {
                Vec::new()
            } else {
                trusted_text
                    .split(',')
                    .filter(|c| !c.trim().is_empty())
                    .map(Cidr::parse)
                    .collect::<Result<Vec<_>>>()?
            };
            let default_role = Role::parse(&cli.auth_default_role)
                .with_context(|| format!("unknown default role {:?}", cli.auth_default_role))?;
            Ok(Authenticator::Header(HeaderAuth {
                user_header: header(&cli.auth_user_header)?,
                name_header: cli.auth_name_header.as_deref().map(header).transpose()?,
                groups_header: cli.auth_groups_header.as_deref().map(header).transpose()?,
                roles: RoleMapping::parse(&cli.auth_role_map, default_role)?,
                trusted,
                trust_any_peer,
            }))
        }
        "users" => {
            let identity = identity_for("users")?;
            let sessions = build_sessions(cli, identity.clone(), true)?;
            // The identity provider an admin sets up on the Admin page: its
            // client secret can be kept only under a session secret of the
            // operator's own, so that it survives a restart.
            let sealer = cli
                .session_secret
                .as_deref()
                .map(|secret| Sealer::from_secret(secret.as_bytes()))
                .transpose()?;
            let provider = ProviderSlot::new(
                OidcSettingsStore::Pg(identity.clone()),
                sealer,
                Arc::clone(&sessions),
            );
            Ok(Authenticator::Users(Box::new(
                UsersAuth::new(UserStore::Pg(identity), sessions).with_provider(provider),
            )))
        }
        _ => Ok(Authenticator::Anonymous),
    }
}

/// The `user` commands: the people of the `users` mode live in the catalog,
/// so these open the identity pool with the writer URL and go through the
/// same [`UsersAuth`] the site uses (its validation, hashing, and the
/// ending of sessions on a password change). No server is started.
async fn run_command(cli: &Cli, command: &Command) -> Result<()> {
    let url = cli.writer_url.as_deref().context(
        "a maintenance command needs --writer-url (OKF_PG_WRITER_URL): people, sessions, and MCP \
         tokens live in the catalog",
    )?;
    let writer = identity_pool(cli, url)?;
    match command {
        Command::User(command) => run_user_command(cli, writer, command).await,
        Command::McpToken(command) => {
            probe_tables(&writer, MCP_TOKEN_TABLES).await?;
            let tokens = McpTokens::new(McpTokenStore::Pg(writer), cli.tenant.clone());
            run_mcp_token_command(tokens, command).await
        }
    }
}

async fn run_user_command(cli: &Cli, identity: Db, command: &UserCommand) -> Result<()> {
    probe_tables(&identity, IDENTITY_TABLES).await?;
    let sessions = build_sessions(cli, identity.clone(), false)?;
    let users = UsersAuth::new(UserStore::Pg(identity), sessions);
    let password = read_password()?;
    match command {
        UserCommand::Add { name, role } => {
            let role = Role::parse(role).with_context(|| format!("unknown role {role:?}"))?;
            users.add_user(name, role, &password).await?;
            eprintln!("pgokf-web: added {name} as {}", role.id());
        }
        UserCommand::SetPassword { name } => {
            users.set_password(name, &password).await?;
            eprintln!("pgokf-web: changed the password of {name} and ended their sessions");
        }
    }
    Ok(())
}

/// The Admin page's token actions from a shell, for a stack that runs the
/// MCP endpoint without the UI. A minted token goes to standard output
/// alone, once; everything else to standard error, so a redirect captures
/// exactly the secret and nothing beside it.
async fn run_mcp_token_command(tokens: McpTokens, command: &McpTokenCommand) -> Result<()> {
    match command {
        McpTokenCommand::Mint { name, role } => {
            let name = name.trim();
            let role = McpRole::parse(role)
                .with_context(|| format!("unknown role {role:?}; use reader or builder"))?;
            match tokens.mint(name, role, "cli").await? {
                Minted::Token { token, record } => {
                    println!("{token}");
                    eprintln!(
                        "pgokf-web: minted MCP token {name} ({role}{}); the token above is shown \
                         once and cannot be recovered",
                        record
                            .tenant
                            .as_deref()
                            .map(|tenant| format!(", tenant {tenant}"))
                            .unwrap_or_default()
                    );
                }
                Minted::NameTaken => {
                    bail!(
                        "a token named {name} already exists; revoke it first, or choose another name"
                    )
                }
                Minted::InvalidName(why) => bail!("{why}"),
            }
        }
        McpTokenCommand::List => {
            for token in tokens.list().await? {
                println!(
                    "{}\t{}\t{}\t{}\t{}",
                    token.name,
                    token.role,
                    token.tenant.as_deref().unwrap_or("-"),
                    token.created_by,
                    token.created_at
                );
            }
        }
        McpTokenCommand::Revoke { name } => {
            let name = name.trim();
            if !tokens.revoke(name).await? {
                bail!("no token is named {name}");
            }
            eprintln!(
                "pgokf-web: revoked MCP token {name}; it is refused from the next request on"
            );
        }
    }
    Ok(())
}

/// A password from standard input, so it never sits in a command line.
fn read_password() -> Result<String> {
    let mut password = String::new();
    std::io::stdin()
        .read_to_string(&mut password)
        .context("reading the password from standard input")?;
    let password = password.trim_end_matches(['\r', '\n']);
    if password.is_empty() {
        bail!("the password (read from standard input) is empty");
    }
    Ok(password.to_owned())
}

/// Probe the embeddings endpoint once and compare the vector width with the
/// catalog's `embedding_dim`: a model of the wrong width can never answer a
/// semantic query, so that is a startup error, while an endpoint that is
/// merely unreachable right now is only a warning (searches fall back to
/// lexical results until it answers).
async fn check_embedding_dimension(db: &Db, client: &EmbeddingsClient, model: &str) -> Result<()> {
    let expected = db.config().await?["embedding_dim"].as_i64();
    match client.embed(&["pgokf".to_owned()]).await {
        Ok(vectors) => {
            let width = vectors.first().map(Vec::len).unwrap_or_default();
            if let Some(expected) = expected
                && i64::try_from(width).ok() != Some(expected)
            {
                bail!(
                    "embedding model {model} produces {width}-dimensional vectors but the \
                     catalog's embedding_dim is {expected}; semantic search would never match"
                );
            }
            eprintln!("pgokf-web: embeddings endpoint answered ({width}-d, model {model})");
        }
        Err(error) => eprintln!(
            "pgokf-web: warning: the embeddings endpoint did not answer ({error:#}); semantic \
             and hybrid searches fall back to lexical results until it does"
        ),
    }
    Ok(())
}
