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
mod oidc;
mod routes;
mod store;

use std::io::Read;
use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use axum::http::HeaderName;
use clap::Parser;
use pgokf_companion::embeddings::EmbeddingsClient;

use crate::auth::{Authenticator, Cidr, HeaderAuth, Role, RoleMapping, Sessions, UsersAuth};
use crate::config::{Cli, Command};
use crate::db::{Db, DbConfig};
use crate::routes::App;

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse().normalized();
    if let Some(command) = &cli.command {
        return run_command(command);
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
    let authenticator = build_authenticator(&cli)?;
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

    let app = Arc::new(App {
        db,
        writer,
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

/// The signer of the session this site holds, for the modes that hold
/// one. Without a configured secret a random one is used, which means the
/// sessions end when the process does.
fn build_sessions(cli: &Cli) -> Result<Arc<Sessions>> {
    let secret = if let Some(secret) = &cli.session_secret {
        secret.as_bytes().to_vec()
    } else {
        eprintln!(
            "pgokf-web: warning: no OKF_WEB_SESSION_SECRET; sessions end when the \
             process does"
        );
        auth::random_bytes(32)?
    };
    Ok(Arc::new(Sessions::new(
        secret,
        cli.session_hours.saturating_mul(3_600),
        cli.cookie_secure,
    )?))
}

/// The way people are identified, from the flags.
fn build_authenticator(cli: &Cli) -> Result<Authenticator> {
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
            Ok(Authenticator::Oidc(Box::new(oidc::OidcAuth::new(
                config,
                build_sessions(cli)?,
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
            let path = cli
                .auth_users_file
                .as_deref()
                .context("--auth users needs --auth-users-file")?;
            Ok(Authenticator::Users(UsersAuth::load(
                path,
                build_sessions(cli)?,
            )?))
        }
        _ => Ok(Authenticator::Anonymous),
    }
}

/// A maintenance command: no catalog, no server.
fn run_command(command: &Command) -> Result<()> {
    match command {
        Command::HashPassword { user, role } => {
            if !auth::valid_subject(user) {
                bail!("{user:?} is not a valid user name (letters, digits, . _ - @ +)");
            }
            let role = Role::parse(role).with_context(|| format!("unknown role {role:?}"))?;
            let mut password = String::new();
            std::io::stdin()
                .read_to_string(&mut password)
                .context("reading the password from standard input")?;
            let password = password.trim_end_matches(['\r', '\n']);
            if password.is_empty() {
                bail!("the password (read from standard input) is empty");
            }
            println!("{user}:{}:{}", role.id(), auth::hash_password(password)?);
            Ok(())
        }
    }
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
