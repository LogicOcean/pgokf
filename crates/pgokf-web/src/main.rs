// SPDX-License-Identifier: AGPL-3.0-only
//! `pgokf-web`: the pgokf catalog's web UI and JSON API.
//!
//! A thin, read-only companion: it renders what the `pgokf_reader` role can
//! see through the public SQL API (search, browse, inspect, monitor) and
//! never holds catalogue semantics of its own. Configuration is by flags or
//! `OKF_*` environment variables like the other companions; the companions
//! image ships the binary.

mod config;
mod db;
mod graph;
mod links;
mod markdown;
mod routes;

use std::sync::Arc;

use anyhow::{Context, Result, bail};
use clap::Parser;
use pgokf_companion::embeddings::EmbeddingsClient;

use crate::config::Cli;
use crate::db::{Db, DbConfig};
use crate::routes::App;

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse().normalized();
    cli.validate()?;

    let db = Db::connect(&DbConfig {
        database_url: &cli.database_url,
        force_tls: cli.tls,
        pool_size: cli.pool_size,
        tenant: cli.tenant.as_deref(),
        statement_timeout_ms: cli.statement_timeout_ms,
    })?;
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
        pgokf_pgconn::parse_config(&cli.database_url)
            .ok()
            .and_then(|c| c.get_dbname().map(str::to_owned))
            .unwrap_or_else(|| "pgokf".to_owned())
    });

    let app = Arc::new(App {
        db,
        embedder,
        catalog_name,
        tenant: cli.tenant.clone(),
        version: version.clone(),
    });
    let router = routes::router(app);

    let listener = tokio::net::TcpListener::bind(cli.bind)
        .await
        .with_context(|| format!("binding {}", cli.bind))?;
    eprintln!(
        "pgokf-web: serving catalog {} (pgokf {version}, SQL {sql_version}) on http://{}{}",
        cli.title.as_deref().unwrap_or("(from connection)"),
        cli.bind,
        cli.tenant
            .as_deref()
            .map(|t| format!(" as tenant {t}"))
            .unwrap_or_default()
    );
    let shutdown = pgokf_companion::daemon::shutdown_signal()?;
    axum::serve(listener, router)
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
