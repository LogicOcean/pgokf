// SPDX-License-Identifier: AGPL-3.0-only
//! `pgokf-embed` — the reference embedding-generation companion for `pgokf`.
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
//! Credentials live here — the endpoint URL, model name, and bearer API key all
//! come from the CLI or environment and are **never** hard-coded or written to
//! PostgreSQL. Any OpenAI-compatible server works: OpenAI itself, a local
//! `text-embeddings-inference` or `llama.cpp` server, or a test mock.

// The prose names many products (PostgreSQL, OpenAI, ...); backticking each
// occurrence would harm readability more than it helps.
#![allow(clippy::doc_markdown)]

mod client;
mod db;

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

    /// Bearer API key for the endpoint. Optional — a local server may need
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
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    run(cli).await
}

/// Connect, resolve the target dimension, find concepts that need an embedding,
/// and embed them in batches.
async fn run(cli: Cli) -> Result<()> {
    if cli.batch_size == 0 {
        bail!("--batch-size must be at least 1");
    }
    if cli.max_chars == 0 {
        bail!("--max-chars must be at least 1");
    }

    let (pg_client, connection_handle) = pgokf_pgconn::connect(&cli.database_url, cli.tls)
        .await
        .context("connecting to PostgreSQL")?;

    let result = embed_all(&cli, &pg_client).await;

    // Drop the client so the connection future completes, then join it, before
    // surfacing any embedding error.
    drop(pg_client);
    connection_handle
        .await
        .context("joining the PostgreSQL connection task")?;
    result
}

/// The core embedding workflow, factored out so the connection lifecycle in
/// [`run`] stays linear.
async fn embed_all(cli: &Cli, pg_client: &tokio_postgres::Client) -> Result<()> {
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
        println!("pgokf-embed: no concepts need an embedding; nothing to do");
        return Ok(());
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

    println!("pgokf-embed: stored {embedded} embedding(s) of dimension {dim}");
    Ok(())
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
