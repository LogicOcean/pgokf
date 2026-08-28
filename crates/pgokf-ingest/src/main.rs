//! `pgokf-ingest` — the mountless OKF ingestion companion.
//!
//! This standalone binary is the network-facing half of the mountless
//! deployment topology. The `pgokf` PostgreSQL extension never performs network
//! I/O; this process does: it reads an OKF bundle from an S3-compatible object
//! store (AWS S3, MinIO, SeaweedFS, Ceph, or GCS/Azure through their S3
//! surface), derives each object's bundle-relative path, and streams the
//! collected `(path, bytes)` into PostgreSQL by calling
//! `pgokf.register_bundle_content(name, paths[], contents[])` as a
//! `pgokf_writer`-capable role.
//!
//! Object-store credentials live here — in the companion's environment, CLI, or
//! an attached IAM instance profile — and never touch PostgreSQL. The server
//! only ever receives the bytes this process hands it, and diffs them against
//! the bundle's stored projection, so re-running the companion is an
//! incremental resync (changed concepts upserted, removed ones deleted).

// The prose in this crate names many products (PostgreSQL, MinIO, SeaweedFS,
// S3, AWS, IAM, ...); backticking each occurrence would harm readability more
// than it helps, so the pedantic doc-markdown lint is relaxed crate-wide.
#![allow(clippy::doc_markdown)]

use anyhow::{Context, Result, bail};
use clap::Parser;
use futures::{StreamExt, TryStreamExt};
use object_store::aws::AmazonS3Builder;
use object_store::path::Path as ObjectPath;
use object_store::{ObjectStore, ObjectStoreExt};
use tokio_postgres::NoTls;

/// Command-line / environment configuration for one ingestion run.
///
/// Every object-store credential can be supplied through the environment (the
/// standard `AWS_*` variables, honored via [`AmazonS3Builder::from_env`], which
/// also resolves an EC2/ECS instance-profile or IAM role when no static keys
/// are set) or overridden on the command line. Nothing is ever hard-coded.
#[derive(Debug, Parser)]
#[command(
    name = "pgokf-ingest",
    about = "Stream an OKF bundle from an S3-compatible object store into pgokf (mountless ingestion)."
)]
struct Cli {
    /// S3 bucket that holds the OKF bundle.
    #[arg(long, env = "OKF_S3_BUCKET")]
    bucket: String,

    /// Key prefix under which the bundle objects live (for example
    /// `handbook/`). The prefix is stripped to derive each bundle-relative
    /// path. Empty means the whole bucket.
    #[arg(long, env = "OKF_S3_PREFIX", default_value = "")]
    prefix: String,

    /// Object-store endpoint URL. Required for MinIO / SeaweedFS / Ceph and any
    /// non-AWS S3 endpoint; omit for real AWS S3 (the region selects it).
    #[arg(long, env = "OKF_S3_ENDPOINT")]
    endpoint: Option<String>,

    /// Object-store region (defaults to `us-east-1`, which MinIO accepts).
    #[arg(long, env = "AWS_REGION", default_value = "us-east-1")]
    region: String,

    /// Allow a plain-HTTP endpoint (needed for a local MinIO on `http://`).
    #[arg(long, env = "OKF_S3_ALLOW_HTTP", default_value_t = false)]
    allow_http: bool,

    /// Static access key id. Prefer the environment / instance profile; this
    /// override exists for ad-hoc runs and is never logged.
    #[arg(long, env = "AWS_ACCESS_KEY_ID", hide_env_values = true)]
    access_key_id: Option<String>,

    /// Static secret access key. Prefer the environment / instance profile;
    /// this override exists for ad-hoc runs and is never logged.
    #[arg(long, env = "AWS_SECRET_ACCESS_KEY", hide_env_values = true)]
    secret_access_key: Option<String>,

    /// PostgreSQL connection string for a `pgokf_writer`-capable role (for
    /// example `postgresql://ingest@db.internal/app`). Never carries
    /// object-store credentials.
    #[arg(long, env = "OKF_PG_URL", hide_env_values = true)]
    database_url: String,

    /// The bundle name to register the content under. The bundle is keyed in
    /// PostgreSQL as `content:<name>`; re-running with the same name resyncs.
    #[arg(long, env = "OKF_BUNDLE_NAME")]
    bundle_name: String,

    /// Maximum number of objects downloaded concurrently.
    #[arg(long, env = "OKF_DOWNLOAD_CONCURRENCY", default_value_t = 8)]
    concurrency: usize,
}

/// One collected object: its bundle-relative path and verbatim bytes.
struct BundleObject {
    path: String,
    bytes: Vec<u8>,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    run(cli).await
}

/// Build the object store, collect the bundle, and stream it into PostgreSQL.
async fn run(cli: Cli) -> Result<()> {
    let store = build_object_store(&cli).context("failed to build the S3 object store")?;

    let normalized_prefix = normalize_prefix(&cli.prefix);
    let objects = collect_bundle(&store, normalized_prefix.as_deref(), cli.concurrency)
        .await
        .context("failed to read the bundle from the object store")?;

    if objects.is_empty() {
        bail!(
            "no OKF (.md) objects found under s3://{}/{}",
            cli.bucket,
            cli.prefix
        );
    }

    eprintln!(
        "pgokf-ingest: collected {} object(s) from s3://{}/{}",
        objects.len(),
        cli.bucket,
        normalized_prefix.unwrap_or_default()
    );

    register_content(&cli, objects)
        .await
        .context("failed to register the bundle content in PostgreSQL")
}

/// Construct the S3 object store from the environment (including an instance
/// profile / IAM role) with CLI overrides layered on top.
fn build_object_store(cli: &Cli) -> Result<impl ObjectStore> {
    // `from_env` seeds credentials from the standard AWS_* variables and, when
    // none are set, from an EC2/ECS instance profile or assumed IAM role — so a
    // production companion needs no static keys at all.
    let mut builder = AmazonS3Builder::from_env()
        .with_bucket_name(&cli.bucket)
        .with_region(&cli.region);

    if let Some(endpoint) = &cli.endpoint {
        builder = builder.with_endpoint(endpoint);
    }
    if cli.allow_http {
        builder = builder.with_allow_http(true);
    }
    if let Some(access_key_id) = &cli.access_key_id {
        builder = builder.with_access_key_id(access_key_id);
    }
    if let Some(secret_access_key) = &cli.secret_access_key {
        builder = builder.with_secret_access_key(secret_access_key);
    }

    builder.build().map_err(anyhow::Error::from)
}

/// Normalize a caller-supplied prefix to `None` (whole bucket) or a
/// trailing-slash-free `Some(prefix)` for object-store listing and stripping.
fn normalize_prefix(prefix: &str) -> Option<String> {
    let trimmed = prefix.trim_matches('/');
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_owned())
    }
}

/// List every `.md` object under `prefix` and download it, deriving each
/// object's bundle-relative path by stripping the prefix.
async fn collect_bundle<S: ObjectStore>(
    store: &S,
    prefix: Option<&str>,
    concurrency: usize,
) -> Result<Vec<BundleObject>> {
    let list_prefix = prefix.map(ObjectPath::from);
    let mut listing = store.list(list_prefix.as_ref());

    let mut locations = Vec::new();
    while let Some(meta) = listing.next().await {
        let meta = meta.context("listing object store")?;
        let location = meta.location.to_string();
        // Mirror OKF bundle discovery: only Markdown files are concepts (the
        // reserved index.md / log.md are handled server-side). Skipping other
        // objects keeps a stray non-Markdown file from aborting a strict sync.
        if location.to_ascii_lowercase().ends_with(".md") {
            locations.push(meta.location);
        }
    }

    let strip = prefix.map(|value| format!("{value}/"));
    let downloads = futures::stream::iter(locations.into_iter().map(|location| {
        let strip = strip.clone();
        async move {
            let bytes = store
                .get(&location)
                .await
                .with_context(|| format!("downloading {location}"))?
                .bytes()
                .await
                .with_context(|| format!("reading {location}"))?;
            let full = location.to_string();
            let relative = match &strip {
                Some(prefix) => full.strip_prefix(prefix).unwrap_or(&full).to_owned(),
                None => full,
            };
            Ok::<_, anyhow::Error>(BundleObject {
                path: relative,
                bytes: bytes.to_vec(),
            })
        }
    }))
    .buffer_unordered(concurrency.max(1));

    let mut objects: Vec<BundleObject> = downloads.try_collect().await?;
    // A deterministic order keeps runs reproducible and logs readable; the
    // server diffs by content, so order does not affect correctness.
    objects.sort_by(|left, right| left.path.cmp(&right.path));
    Ok(objects)
}

/// Connect to PostgreSQL and call `pgokf.register_bundle_content`, printing the
/// per-bucket sync counts it returns.
async fn register_content(cli: &Cli, objects: Vec<BundleObject>) -> Result<()> {
    let (paths, contents): (Vec<String>, Vec<Vec<u8>>) =
        objects.into_iter().map(|o| (o.path, o.bytes)).unzip();

    let (client, connection) = tokio_postgres::connect(&cli.database_url, NoTls)
        .await
        .context("connecting to PostgreSQL")?;
    // The connection future drives the protocol; run it in the background and
    // surface any transport error.
    let connection_handle = tokio::spawn(async move {
        if let Err(error) = connection.await {
            eprintln!("pgokf-ingest: PostgreSQL connection error: {error}");
        }
    });

    let row = client
        .query_one(
            "SELECT bundle_id, added, updated, removed, unchanged, total
             FROM pgokf.register_bundle_content($1, $2, $3) AS r",
            &[&cli.bundle_name, &paths, &contents],
        )
        .await
        .context("calling pgokf.register_bundle_content")?;

    let bundle_id: i64 = row.get("bundle_id");
    let added: i32 = row.get("added");
    let updated: i32 = row.get("updated");
    let removed: i32 = row.get("removed");
    let unchanged: i32 = row.get("unchanged");
    let total: i32 = row.get("total");

    println!(
        "pgokf-ingest: registered content bundle '{}' (bundle_id={bundle_id}, source_type=content)\n\
         \tadded={added} updated={updated} removed={removed} unchanged={unchanged} total={total}",
        cli.bundle_name,
    );

    // Drop the client so the connection future completes, then join it.
    drop(client);
    connection_handle
        .await
        .context("joining the PostgreSQL connection task")?;
    Ok(())
}
