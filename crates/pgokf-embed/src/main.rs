// SPDX-License-Identifier: AGPL-3.0-only
//! `pgokf-embed` - the reference embedding-generation companion for `pgokf`.
//!
//! This standalone async binary is the embedder half of the shipped semantic
//! search feature. The `pgokf` extension stores caller-computed embedding
//! vectors (via `pgokf.set_concept_embedding`) and ranks concepts against a
//! query vector (via `pgokf.concept_search_semantic`), but it never computes an
//! embedding and never performs network I/O. This companion closes that loop:
//! it finds concepts that lack an embedding, computes one for each against a
//! configurable OpenAI-compatible embeddings endpoint, and streams the vectors
//! back through the setter.
//!
//! It runs once by default, or as a daemon with `--watch`: every `--interval`
//! seconds it looks again for concepts without a vector (newly registered or
//! refreshed content) and embeds them, until SIGINT / SIGTERM.
//!
//! Credentials live here - the endpoint URL, model name, and bearer API key all
//! come from the CLI or environment and are **never** hard-coded or written to
//! PostgreSQL. Any OpenAI-compatible server works: OpenAI itself, a local
//! Ollama, `text-embeddings-inference`, or `llama.cpp` server, or a test mock.

// The prose names many products (PostgreSQL, OpenAI, ...); backticking each
// occurrence would harm readability more than it helps.
#![allow(clippy::doc_markdown)]

mod client;
mod db;

use std::time::Duration;

use anyhow::{Context, Result, bail};
use clap::Parser;

use crate::client::EmbeddingsClient;
use crate::db::PendingConcept;

/// Command-line / environment configuration for one embedding run.
///
/// The embeddings endpoint, model, and API key are supplied here (never
/// hard-coded, never stored in PostgreSQL). The database URL authenticates a
/// `pgokf_writer`-capable role.
#[derive(Debug, Parser)]
#[command(
    name = "pgokf-embed",
    about = "Compute concept embeddings against an OpenAI-compatible endpoint and stream them into pgokf."
)]
struct Cli {
    /// PostgreSQL connection string for a `pgokf_writer`-capable role (for
    /// example `postgresql://okf_embed@db.internal/app`).
    #[arg(long, env = "OKF_PG_URL", hide_env_values = true)]
    database_url: String,

    /// Base URL of the OpenAI-compatible embeddings server. The
    /// `/v1/embeddings` path is appended automatically (for example
    /// `https://api.openai.com` or `http://127.0.0.1:8080`).
    #[arg(long, env = "OKF_EMBED_ENDPOINT")]
    endpoint: String,

    /// Embedding model name passed through in the request body (for example
    /// `text-embedding-3-small`).
    #[arg(long, env = "OKF_EMBED_MODEL")]
    model: String,

    /// Bearer API key for the endpoint. Optional - a local server may need
    /// none. Prefer the environment over the command line; never logged.
    #[arg(long, env = "OKF_EMBED_API_KEY", hide_env_values = true)]
    api_key: Option<String>,

    /// Only embed concepts in this bundle id. Omit to embed across all bundles.
    #[arg(long, env = "OKF_EMBED_BUNDLE")]
    bundle: Option<i64>,

    /// Expected embedding dimension. Defaults to the durable `embedding_dim`
    /// configuration key read from `pgokf.get_config`.
    #[arg(long, env = "OKF_EMBED_DIM")]
    dim: Option<i32>,

    /// Number of concepts embedded per HTTP request.
    #[arg(long, env = "OKF_EMBED_BATCH", default_value_t = 32)]
    batch_size: usize,

    /// Maximum characters of `title + description + body_text` sent per concept.
    #[arg(long, env = "OKF_EMBED_MAX_CHARS", default_value_t = 8000)]
    max_chars: usize,

    /// Optional multi-tenant scope applied as `pgokf.tenant` for the session.
    #[arg(long, env = "OKF_TENANT")]
    tenant: Option<String>,

    /// Require a TLS-encrypted link to PostgreSQL. TLS is also enabled by an
    /// `sslmode=require` (or stricter) in the connection URL; otherwise the link
    /// is plaintext (the default, for a local socket / trusted network).
    #[arg(long, env = "OKF_PG_TLS", default_value_t = false)]
    tls: bool,

    /// Run as a daemon: after the initial pass, look for concepts without an
    /// embedding every `--interval` seconds and embed them, until SIGINT or
    /// SIGTERM. A failed pass is logged and retried on the next interval.
    #[arg(long, env = "OKF_WATCH", default_value_t = false)]
    watch: bool,

    /// Poll interval, in seconds, between watch passes (only used with
    /// `--watch`). Must be at least 1.
    #[arg(long, env = "OKF_WATCH_INTERVAL", default_value_t = 60)]
    interval: u64,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse().normalized();
    run(cli).await
}

impl Cli {
    /// Apply the shared rule for optional values that also come from the
    /// environment: empty means unset (see [`pgokf_companion::cli::non_empty`]).
    fn normalized(mut self) -> Self {
        self.api_key = pgokf_companion::cli::non_empty(self.api_key);
        self.tenant = pgokf_companion::cli::non_empty(self.tenant);
        self
    }
}

/// Validate the configuration and dispatch to the one-shot or watch-daemon flow.
async fn run(cli: Cli) -> Result<()> {
    validate(&cli)?;

    if cli.watch {
        run_watch(&cli).await
    } else {
        let embedded = run_pass(&cli).await?;
        if embedded == 0 {
            println!("pgokf-embed: no concepts need an embedding; nothing to do");
        } else {
            println!("pgokf-embed: stored {embedded} embedding(s)");
        }
        Ok(())
    }
}

/// Reject configurations that could only fail later or loop uselessly.
fn validate(cli: &Cli) -> Result<()> {
    if cli.batch_size == 0 {
        bail!("--batch-size must be at least 1");
    }
    if cli.max_chars == 0 {
        bail!("--max-chars must be at least 1");
    }
    if cli.watch && cli.interval == 0 {
        bail!("--interval must be at least 1 second");
    }
    Ok(())
}

/// Watch daemon: embed whatever is pending now, then again every interval,
/// until SIGINT / SIGTERM. Each pass opens its own connection so a database
/// restart between passes is survived rather than poisoning a long-lived link.
async fn run_watch(cli: &Cli) -> Result<()> {
    eprintln!(
        "pgokf-embed: watching for concepts without an embedding every {}s (SIGINT/SIGTERM to stop)",
        cli.interval,
    );

    let shutdown = pgokf_companion::daemon::shutdown_signal()?;
    pgokf_companion::daemon::run(
        Duration::from_secs(cli.interval),
        async || {
            let embedded = run_pass(cli).await?;
            if embedded > 0 {
                eprintln!("pgokf-embed: watch pass stored {embedded} embedding(s)");
            }
            Ok(())
        },
        shutdown,
        |error| eprintln!("pgokf-embed: watch pass failed, retrying next interval: {error:#}"),
    )
    .await?;

    eprintln!("pgokf-embed: shutdown requested, exiting");
    Ok(())
}

/// One complete pass: connect, resolve the target dimension, find concepts
/// that need an embedding, embed them in batches, and close the connection.
/// Returns the number of embeddings stored.
async fn run_pass(cli: &Cli) -> Result<usize> {
    let (pg_client, connection_handle) = pgokf_pgconn::connect(&cli.database_url, cli.tls)
        .await
        .context("connecting to PostgreSQL")?;

    let result = embed_all(cli, &pg_client).await;

    // Drop the client so the connection future completes, then join it, before
    // surfacing any embedding error.
    drop(pg_client);
    connection_handle
        .await
        .context("joining the PostgreSQL connection task")?;
    result
}

/// The core embedding workflow, factored out so the connection lifecycle in
/// [`run_pass`] stays linear. Returns the number of embeddings stored.
async fn embed_all(cli: &Cli, pg_client: &tokio_postgres::Client) -> Result<usize> {
    if let Some(tenant) = &cli.tenant {
        db::set_tenant(pg_client, tenant).await?;
    }

    let dim = match cli.dim {
        Some(dim) => dim,
        None => db::embedding_dim(pg_client)
            .await
            .context("resolving embedding_dim (pass --dim to override)")?,
    };

    let pending = db::pending_concepts(pg_client, cli.bundle).await?;
    if pending.is_empty() {
        return Ok(0);
    }

    eprintln!(
        "pgokf-embed: {} concept(s) need an embedding (dim={dim}, batch={}, model={})",
        pending.len(),
        cli.batch_size,
        cli.model,
    );

    let embeddings_client =
        EmbeddingsClient::new(&cli.endpoint, cli.model.clone(), cli.api_key.clone())
            .context("building the embeddings client")?;

    let mut embedded = 0_usize;
    for batch in pending.chunks(cli.batch_size) {
        embedded += embed_batch(cli, pg_client, &embeddings_client, dim, batch).await?;
        eprintln!(
            "pgokf-embed: embedded {embedded}/{} concept(s)",
            pending.len()
        );
    }

    Ok(embedded)
}

/// Embed one batch of concepts and store each returned vector. Returns the
/// number of vectors stored.
async fn embed_batch(
    cli: &Cli,
    pg_client: &tokio_postgres::Client,
    embeddings_client: &EmbeddingsClient,
    dim: i32,
    batch: &[PendingConcept],
) -> Result<usize> {
    let inputs: Vec<String> = batch
        .iter()
        .map(|concept| concept.embedding_input(cli.max_chars))
        .collect();

    let vectors = embeddings_client
        .embed(&inputs)
        .await
        .context("calling the embeddings endpoint")?;

    for (concept, vector) in batch.iter().zip(vectors) {
        let actual = i32::try_from(vector.len()).unwrap_or(i32::MAX);
        if actual != dim {
            bail!(
                "endpoint returned a {actual}-dimension vector for concept '{}', but embedding_dim is {dim}",
                concept.concept_id,
            );
        }
        db::store_embedding(pg_client, concept.bundle_id, &concept.concept_id, &vector).await?;
    }

    Ok(batch.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Parse a command line the way `main` does, with the required options
    /// supplied so only the arguments under test vary.
    fn parse(extra: &[&str]) -> Cli {
        let mut args = vec![
            "pgokf-embed",
            "--database-url",
            "postgresql://okf_embed@localhost/app",
            "--endpoint",
            "http://127.0.0.1:8080",
            "--model",
            "test-model",
        ];
        args.extend_from_slice(extra);
        Cli::parse_from(args)
    }

    #[test]
    fn normalized_treats_empty_optional_values_as_unset() {
        // Arrange: the shape a compose stack produces for an unset variable.
        let cli = parse(&["--api-key", "", "--tenant", ""]);

        // Act
        let cli = cli.normalized();

        // Assert
        assert_eq!(cli.api_key, None);
        assert_eq!(cli.tenant, None);
    }

    #[test]
    fn validate_accepts_the_defaults() {
        // Arrange
        let cli = parse(&[]);

        // Act / Assert
        assert!(validate(&cli).is_ok());
        assert!(!cli.watch);
        assert_eq!(cli.interval, 60);
    }

    #[test]
    fn validate_rejects_a_zero_watch_interval() {
        // Arrange
        let cli = parse(&["--watch", "--interval", "0"]);

        // Act
        let error = validate(&cli).expect_err("zero interval must be rejected");

        // Assert
        assert_eq!(error.to_string(), "--interval must be at least 1 second");
    }

    #[test]
    fn validate_ignores_the_interval_without_watch() {
        // Arrange: an interval of 0 is meaningless but harmless in one-shot mode.
        let cli = parse(&["--interval", "0"]);

        // Act / Assert
        assert!(validate(&cli).is_ok());
    }

    #[test]
    fn validate_rejects_an_empty_batch_or_input_bound() {
        // Arrange
        let no_batch = parse(&["--batch-size", "0"]);
        let no_chars = parse(&["--max-chars", "0"]);

        // Act / Assert
        assert_eq!(
            validate(&no_batch).expect_err("zero batch").to_string(),
            "--batch-size must be at least 1"
        );
        assert_eq!(
            validate(&no_chars).expect_err("zero chars").to_string(),
            "--max-chars must be at least 1"
        );
    }
}
