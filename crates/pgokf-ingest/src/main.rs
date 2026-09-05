// SPDX-License-Identifier: AGPL-3.0-only
//! `pgokf-ingest` - the mountless OKF ingestion companion.
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
//! Object-store credentials live here - in the companion's environment, CLI, or
//! an attached IAM instance profile - and never touch PostgreSQL. The server
//! only ever receives the bytes this process hands it, and diffs them against
//! the bundle's stored projection, so re-running the companion is an
//! incremental resync (changed concepts upserted, removed ones deleted).

// The prose in this crate names many products (PostgreSQL, MinIO, SeaweedFS,
// S3, AWS, IAM, ...); backticking each occurrence would harm readability more
// than it helps, so the pedantic doc-markdown lint is relaxed crate-wide.
#![allow(clippy::doc_markdown)]

use std::time::Duration;

use anyhow::{Context, Result, bail};
use clap::Parser;
use futures::{StreamExt, TryStreamExt};
use object_store::aws::AmazonS3Builder;
use object_store::path::Path as ObjectPath;
use object_store::{ObjectStore, ObjectStoreExt};

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

    /// Optional multi-tenant scope applied as `pgokf.tenant` for the session:
    /// the bundle is registered under this tenant. Required once the catalog's
    /// `require_tenant` policy is on; an empty value means unset.
    #[arg(long, env = "OKF_TENANT")]
    tenant: Option<String>,

    /// Maximum number of objects downloaded concurrently.
    #[arg(long, env = "OKF_DOWNLOAD_CONCURRENCY", default_value_t = 8)]
    concurrency: usize,

    /// Run as a daemon: after the initial sync, re-list the object store every
    /// `--interval` seconds and re-ingest when the collected content changed.
    /// Omit for the default one-shot sync. Stops cleanly on SIGINT (Ctrl-C).
    #[arg(long, env = "OKF_WATCH", default_value_t = false)]
    watch: bool,

    /// Poll interval, in seconds, between watch passes (only used with
    /// `--watch`). Must be at least 1.
    #[arg(long, env = "OKF_WATCH_INTERVAL", default_value_t = 60)]
    interval: u64,

    /// Require a TLS-encrypted link to PostgreSQL. TLS is also enabled by an
    /// `sslmode=require` (or stricter) in the connection URL; otherwise the link
    /// is plaintext (the default, for a local socket / trusted network). Object-
    /// store TLS is independent and unaffected by this flag.
    #[arg(long, env = "OKF_PG_TLS", default_value_t = false)]
    tls: bool,
}

/// One collected object: its bundle-relative path and verbatim bytes.
struct BundleObject {
    path: String,
    bytes: Vec<u8>,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse().normalized();
    run(cli).await
}

impl Cli {
    /// Apply the shared rule for optional values that also come from the
    /// environment: empty means unset (see [`pgokf_companion::cli::non_empty`]),
    /// so an empty endpoint or static key never overrides the AWS defaults.
    fn normalized(mut self) -> Self {
        self.endpoint = pgokf_companion::cli::non_empty(self.endpoint);
        self.tenant = pgokf_companion::cli::non_empty(self.tenant);
        self.access_key_id = pgokf_companion::cli::non_empty(self.access_key_id);
        self.secret_access_key = pgokf_companion::cli::non_empty(self.secret_access_key);
        self
    }
}

/// Build the object store and dispatch to the one-shot or watch-daemon flow.
async fn run(cli: Cli) -> Result<()> {
    if cli.watch && cli.interval == 0 {
        bail!("--interval must be at least 1 second");
    }

    let store = build_object_store(&cli).context("failed to build the S3 object store")?;
    let normalized_prefix = normalize_prefix(&cli.prefix);

    if cli.watch {
        run_watch(&cli, &store, normalized_prefix.as_deref()).await
    } else {
        run_once(&cli, &store, normalized_prefix.as_deref()).await
    }
}

/// One-shot sync: collect the bundle and stream it into PostgreSQL. Empty is a
/// hard error, matching the original single-run behavior.
async fn run_once<S: ObjectStore>(
    cli: &Cli,
    store: &S,
    normalized_prefix: Option<&str>,
) -> Result<()> {
    let objects = collect_bundle(store, normalized_prefix, cli.concurrency)
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

    register_content(cli, objects)
        .await
        .context("failed to register the bundle content in PostgreSQL")
}

/// Watch daemon: run an initial sync, then re-list and re-ingest on each
/// interval, skipping passes whose collected content is byte-identical to the
/// previous one. Runs until SIGINT or SIGTERM, then returns cleanly.
///
/// A transient error in one pass (a listing/download hiccup, a momentary
/// database outage) is logged and the daemon keeps polling; it does not abort
/// the whole watch. The initial collect is deliberately allowed to see an empty
/// bundle without erroring, because a watched bundle may legitimately be
/// populated after the daemon starts.
async fn run_watch<S: ObjectStore>(
    cli: &Cli,
    store: &S,
    normalized_prefix: Option<&str>,
) -> Result<()> {
    eprintln!(
        "pgokf-ingest: watching s3://{}/{} every {}s (SIGINT/SIGTERM to stop)",
        cli.bucket,
        normalized_prefix.unwrap_or_default(),
        cli.interval,
    );

    let mut last_hash: Option<[u8; blake3::OUT_LEN]> = None;
    let shutdown = pgokf_companion::daemon::shutdown_signal()?;
    pgokf_companion::daemon::run(
        Duration::from_secs(cli.interval),
        async || {
            // No change (or an empty bundle) keeps the prior fingerprint.
            if let Some(hash) = sync_pass(cli, store, normalized_prefix, last_hash).await? {
                last_hash = Some(hash);
            }
            Ok(())
        },
        shutdown,
        |error| eprintln!("pgokf-ingest: watch pass failed, retrying next interval: {error:#}"),
    )
    .await?;

    eprintln!("pgokf-ingest: shutdown requested, exiting");
    Ok(())
}

/// Run one watch pass: collect the current object set, and register it only
/// when its content hash differs from `last_hash`. Returns the new hash when a
/// (re)sync happened, or `None` when the pass was skipped (unchanged) or the
/// bundle was empty.
async fn sync_pass<S: ObjectStore>(
    cli: &Cli,
    store: &S,
    normalized_prefix: Option<&str>,
    last_hash: Option<[u8; blake3::OUT_LEN]>,
) -> Result<Option<[u8; blake3::OUT_LEN]>> {
    let objects = collect_bundle(store, normalized_prefix, cli.concurrency)
        .await
        .context("failed to read the bundle from the object store")?;

    if objects.is_empty() {
        eprintln!(
            "pgokf-ingest: no OKF (.md) objects under s3://{}/{}; waiting",
            cli.bucket,
            normalized_prefix.unwrap_or_default(),
        );
        return Ok(None);
    }

    let hash = hash_objects(&objects);
    if Some(hash) == last_hash {
        eprintln!(
            "pgokf-ingest: {} object(s) unchanged; skipping resync",
            objects.len(),
        );
        return Ok(None);
    }

    eprintln!(
        "pgokf-ingest: collected {} object(s) from s3://{}/{}",
        objects.len(),
        cli.bucket,
        normalized_prefix.unwrap_or_default(),
    );
    register_content(cli, objects)
        .await
        .context("failed to register the bundle content in PostgreSQL")?;
    Ok(Some(hash))
}

/// Fingerprint the collected object set so an unchanged watch pass can skip the
/// PostgreSQL round-trip. Objects arrive sorted by path (see [`collect_bundle`]),
/// so the digest is order-stable; length prefixes make the concatenation
/// unambiguous (no path/content boundary can be forged by concatenation).
fn hash_objects(objects: &[BundleObject]) -> [u8; blake3::OUT_LEN] {
    let mut hasher = blake3::Hasher::new();
    for object in objects {
        hasher.update(&(object.path.len() as u64).to_le_bytes());
        hasher.update(object.path.as_bytes());
        hasher.update(&(object.bytes.len() as u64).to_le_bytes());
        hasher.update(&object.bytes);
    }
    *hasher.finalize().as_bytes()
}

/// Construct the S3 object store from the environment (including an instance
/// profile / IAM role) with CLI overrides layered on top.
fn build_object_store(cli: &Cli) -> Result<impl ObjectStore> {
    // `from_env` seeds credentials from the standard AWS_* variables and, when
    // none are set, from an EC2/ECS instance profile or assumed IAM role - so a
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
    let locations = list_markdown_locations(store, prefix).await?;
    let strip = prefix.map(|value| format!("{value}/"));
    let downloads = futures::stream::iter(
        locations
            .into_iter()
            .map(|location| download_object(store, location, strip.clone())),
    )
    .buffer_unordered(concurrency.max(1));

    let mut objects: Vec<BundleObject> = downloads.try_collect().await?;
    // A deterministic order keeps runs reproducible and logs readable; the
    // server diffs by content, so order does not affect correctness.
    objects.sort_by(|left, right| left.path.cmp(&right.path));
    Ok(objects)
}

/// List every Markdown object under `prefix`, in object-store listing order.
///
/// Only `.md` objects are concepts (the reserved `index.md` / `log.md` are
/// handled server-side); skipping other objects keeps a stray non-Markdown file
/// from aborting a strict sync.
async fn list_markdown_locations<S: ObjectStore>(
    store: &S,
    prefix: Option<&str>,
) -> Result<Vec<ObjectPath>> {
    let list_prefix = prefix.map(ObjectPath::from);
    let mut listing = store.list(list_prefix.as_ref());

    let mut locations = Vec::new();
    while let Some(meta) = listing.next().await {
        let meta = meta.context("listing object store")?;
        if meta
            .location
            .to_string()
            .to_ascii_lowercase()
            .ends_with(".md")
        {
            locations.push(meta.location);
        }
    }
    Ok(locations)
}

/// Download one object and derive its bundle-relative path by stripping the
/// listing prefix.
async fn download_object<S: ObjectStore>(
    store: &S,
    location: ObjectPath,
    strip: Option<String>,
) -> Result<BundleObject> {
    let bytes = store
        .get(&location)
        .await
        .with_context(|| format!("downloading {location}"))?
        .bytes()
        .await
        .with_context(|| format!("reading {location}"))?;
    let full = location.to_string();
    Ok(BundleObject {
        path: relative_path(&full, strip.as_deref()),
        bytes: bytes.to_vec(),
    })
}

/// Derive an object's bundle-relative path by stripping the listing prefix.
///
/// The strip value, when present, is the normalized prefix plus a trailing
/// slash (built in [`collect_bundle`]), so stripping it turns
/// `handbook/topics/a.md` into the bundle-relative `topics/a.md`. `None` (a
/// whole-bucket listing) keeps the key verbatim. An object whose key does not
/// start with the prefix - which the prefixed listing should never surface - is
/// returned whole rather than mangled.
fn relative_path(full: &str, strip: Option<&str>) -> String {
    match strip {
        Some(prefix) => full.strip_prefix(prefix).unwrap_or(full).to_owned(),
        None => full.to_owned(),
    }
}

/// Connect to PostgreSQL and call `pgokf.register_bundle_content`, printing the
/// per-bucket sync counts it returns.
async fn register_content(cli: &Cli, objects: Vec<BundleObject>) -> Result<()> {
    let (paths, contents): (Vec<String>, Vec<Vec<u8>>) =
        objects.into_iter().map(|o| (o.path, o.bytes)).unzip();

    // The shared helper opens the link (NoTls by default; rustls TLS when --tls
    // is set or the URL requires it) and drives the connection's protocol future
    // on the returned background task.
    let (client, connection_handle) = pgokf_pgconn::connect(&cli.database_url, cli.tls)
        .await
        .context("connecting to PostgreSQL")?;
    if let Some(tenant) = &cli.tenant {
        pgokf_pgconn::set_tenant(&client, tenant).await?;
    }

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

#[cfg(test)]
mod tests {
    use super::{BundleObject, Cli, hash_objects, normalize_prefix, relative_path};
    use clap::Parser;

    /// Parse the CLI with the required arguments supplied and `extra` appended.
    fn parse(extra: &[&str]) -> Cli {
        let mut args = vec![
            "pgokf-ingest",
            "--bucket",
            "okf-bundles",
            "--database-url",
            "postgresql://ingest@localhost/app",
            "--bundle-name",
            "handbook",
        ];
        args.extend_from_slice(extra);
        Cli::parse_from(args)
    }

    #[test]
    fn normalized_treats_an_empty_tenant_as_unset() {
        // Arrange: the shape the compose stack produces for an unset OKF_TENANT.
        let cli = parse(&["--tenant", ""]);

        // Act
        let cli = cli.normalized();

        // Assert
        assert_eq!(cli.tenant, None);
    }

    #[test]
    fn normalized_keeps_a_set_tenant() {
        // Arrange
        let cli = parse(&["--tenant", "acme"]);

        // Act
        let cli = cli.normalized();

        // Assert
        assert_eq!(cli.tenant.as_deref(), Some("acme"));
    }

    fn object(path: &str, bytes: &[u8]) -> BundleObject {
        BundleObject {
            path: path.to_owned(),
            bytes: bytes.to_vec(),
        }
    }

    #[test]
    fn relative_path_strips_the_listing_prefix() {
        // Arrange: a nested key under a `handbook/` prefix.
        let full = "handbook/topics/alpha.md";

        // Act
        let relative = relative_path(full, Some("handbook/"));

        // Assert: the prefix is removed, leaving the bundle-relative path.
        assert_eq!(relative, "topics/alpha.md");
    }

    #[test]
    fn relative_path_keeps_the_whole_key_without_a_prefix() {
        // Arrange / Act: a whole-bucket listing passes no strip prefix.
        let relative = relative_path("alpha.md", None);

        // Assert
        assert_eq!(relative, "alpha.md");
    }

    #[test]
    fn relative_path_leaves_a_non_matching_key_unmangled() {
        // Arrange: a key that does not start with the strip prefix (which a
        // prefixed listing should never surface) is returned whole, not
        // truncated mid-segment.
        let full = "other/alpha.md";

        // Act
        let relative = relative_path(full, Some("handbook/"));

        // Assert
        assert_eq!(relative, "other/alpha.md");
    }

    #[test]
    fn normalize_prefix_trims_slashes_and_empties_to_none() {
        // Arrange / Act / Assert: surrounding slashes are trimmed, and an
        // effectively empty prefix normalizes to a whole-bucket listing.
        assert_eq!(normalize_prefix("/handbook/"), Some("handbook".to_owned()));
        assert_eq!(normalize_prefix("handbook"), Some("handbook".to_owned()));
        assert_eq!(normalize_prefix(""), None);
        assert_eq!(normalize_prefix("///"), None);
    }

    #[test]
    fn hash_objects_is_stable_for_identical_content() {
        // Arrange: two independently built but byte-identical object sets.
        let first = [object("a.md", b"alpha"), object("b.md", b"beta")];
        let second = [object("a.md", b"alpha"), object("b.md", b"beta")];

        // Act / Assert: an unchanged pass fingerprints identically, so the watch
        // daemon can skip the resync.
        assert_eq!(hash_objects(&first), hash_objects(&second));
    }

    #[test]
    fn hash_objects_changes_when_content_changes() {
        // Arrange: same paths, one differing body.
        let before = [object("a.md", b"alpha"), object("b.md", b"beta")];
        let after = [object("a.md", b"alpha"), object("b.md", b"BETA")];

        // Act / Assert: a content change flips the fingerprint, forcing a resync.
        assert_ne!(hash_objects(&before), hash_objects(&after));
    }

    #[test]
    fn hash_objects_is_unambiguous_across_the_path_content_boundary() {
        // Arrange: the same concatenated bytes split differently between path and
        // content. Length-prefixing must keep the two fingerprints distinct so a
        // boundary shift cannot be forged into a "no change" pass.
        let left_split = [object("a.md", b"xbeta")];
        let right_split = [object("a.mdx", b"beta")];

        // Act / Assert
        assert_ne!(hash_objects(&left_split), hash_objects(&right_split));
    }
}
