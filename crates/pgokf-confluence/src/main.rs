// SPDX-License-Identifier: AGPL-3.0-only
//! `pgokf-confluence` - the Confluence Cloud ingestion companion.
//!
//! One run reads every current page visible to the configured token in one
//! Confluence Cloud space, converts the complete space to OKF Markdown, and -
//! only with `--apply` - replaces a dedicated content bundle in one PostgreSQL
//! transaction through `pgokf.register_bundle_content_with_context`, as a
//! `pgokf_writer`-capable role. Without `--apply` the run is a read-only
//! preview: it reports the page count, conversion warnings, and the page IDs a
//! commit would remove, and never connects to PostgreSQL.
//!
//! Like the other companions, the extension performs no network I/O; this
//! process does. The Confluence token lives only in this process (a file or
//! `CONFLUENCE_API_TOKEN`, never a command-line argument) and never reaches
//! PostgreSQL. A durable checkpoint (`--state-file`) binds the run to one
//! source, bundle, tenant, and database endpoint, and turns page removals into
//! an explicit, reviewed decision (`--allow-deletions`).

// CLI doc comments render as plain help text, and the prose names products
// (Confluence, PostgreSQL, Atlassian, ...); backticking each occurrence would
// harm readability, so the pedantic doc-markdown lint is relaxed here.
#![allow(clippy::doc_markdown)]

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, ensure};
use clap::Parser;
use pgokf_companion::cli::non_empty;
use pgokf_confluence::client::{Client, ClientConfig};
use pgokf_confluence::convert::convert_pages;
use pgokf_confluence::sink::Catalog;
use pgokf_confluence::snapshot::{self, Snapshot, StateLock};
use pgokf_confluence::types::SourceIdentity;
use serde_json::json;

/// The tenant a run is bound to when none is configured; the catalog's own
/// effective tenant for a session without `pgokf.tenant`.
const DEFAULT_TENANT: &str = "default";
/// Upper bound of `--max-retries`, matching the client's own ceiling.
const MAX_RETRIES_LIMIT: u32 = 10;
/// Largest token file accepted; Atlassian API tokens are far smaller.
const MAX_TOKEN_FILE_BYTES: u64 = 64 * 1024;

/// Import the pages visible to one token from a shared-audience Confluence Cloud space.
#[derive(Parser)]
#[command(
    name = "pgokf-confluence",
    version,
    about = "Import one Confluence Cloud space into pgokf as a complete OKF content bundle (dry run unless --apply)."
)]
struct Cli {
    /// Canonical HTTPS tenant URL, for example https://example.atlassian.net/wiki.
    #[arg(long, env = "CONFLUENCE_URL")]
    site_url: String,
    /// Atlassian account email for API-token Basic authentication.
    #[arg(long, env = "CONFLUENCE_EMAIL")]
    email: String,
    /// API token file; otherwise CONFLUENCE_API_TOKEN (never a token CLI argument).
    #[arg(long, env = "CONFLUENCE_TOKEN_FILE")]
    token_file: Option<String>,
    /// Cloud UUID for scoped tokens routed through api.atlassian.com.
    #[arg(long, env = "CONFLUENCE_CLOUD_ID")]
    cloud_id: Option<String>,
    /// Numeric Confluence space ID (not the space key).
    #[arg(long, env = "CONFLUENCE_SPACE_ID")]
    space_id: String,
    /// Dedicated content-bundle name. Never reuse a manually maintained bundle.
    #[arg(long, env = "OKF_BUNDLE_NAME")]
    bundle_name: String,
    /// PostgreSQL writer connection string; required for --apply.
    #[arg(long, env = "OKF_PG_URL", hide_env_values = true)]
    database_url: Option<String>,
    /// Require a TLS-encrypted, certificate-verified link to PostgreSQL;
    /// sslmode=require in the connection string also enables it.
    #[arg(long, env = "OKF_PG_TLS", default_value_t = false)]
    tls: bool,
    /// Catalog tenant the bundle is registered under (default: "default").
    /// Required once the catalog's require_tenant policy is on; a scoping
    /// selector, not an authorization boundary. An empty value means unset.
    #[arg(long, env = "OKF_TENANT")]
    tenant: Option<String>,
    /// Durable checkpoint path; required for --apply and shared by scheduled runs.
    #[arg(long)]
    state_file: Option<PathBuf>,
    /// Export OKF Markdown and a manifest to a new directory (must not exist).
    #[arg(long)]
    output_dir: Option<PathBuf>,
    /// Commit the complete snapshot to PostgreSQL. Omit for a read-only preview.
    #[arg(long, requires_all = ["database_url", "state_file"])]
    apply: bool,
    /// Explicitly permit all page removals; review removed_page_ids in a fresh dry run first.
    #[arg(long)]
    allow_deletions: bool,
    /// Fail if the space has more pages than this bound; never truncate it.
    #[arg(long, default_value_t = 10_000)]
    max_pages: usize,
    /// Maximum bytes in any single HTTP response.
    #[arg(long, default_value_t = 8 * 1024 * 1024)]
    max_response_bytes: usize,
    /// Maximum retries for each read request (at most 10).
    #[arg(long, default_value_t = 3)]
    max_retries: u32,
}

impl Cli {
    /// Apply the companions' shared rule for optional values that also come
    /// from the environment: empty means unset (see
    /// [`pgokf_companion::cli::non_empty`]), so a container that can only
    /// blank a variable never supplies an empty tenant, URL, or token path.
    fn normalized(mut self) -> Self {
        self.token_file = non_empty(self.token_file);
        self.cloud_id = non_empty(self.cloud_id).map(|id| id.to_ascii_lowercase());
        self.database_url = non_empty(self.database_url);
        self.tenant = non_empty(self.tenant);
        self
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    run(Cli::parse().normalized()).await
}

async fn run(cli: Cli) -> Result<()> {
    ensure!(
        cli.max_pages > 0 && cli.max_response_bytes > 0,
        "page and response limits must be positive"
    );
    ensure!(
        cli.max_retries <= MAX_RETRIES_LIMIT,
        "--max-retries cannot exceed {MAX_RETRIES_LIMIT}"
    );
    let (client, source) = confluence_client(&cli)?;
    let tenant = cli
        .tenant
        .clone()
        .unwrap_or_else(|| DEFAULT_TENANT.to_owned());
    ensure!(
        !tenant.trim().is_empty() && !tenant.contains('\0'),
        "tenant must be nonempty and contain no NUL"
    );
    ensure!(
        !cli.bundle_name.trim().is_empty() && !cli.bundle_name.contains('\0'),
        "bundle name must be nonempty and contain no NUL"
    );
    let _lock = cli
        .state_file
        .as_deref()
        .map(StateLock::acquire)
        .transpose()?;
    let previous = cli
        .state_file
        .as_deref()
        .map(snapshot::read_checkpoint)
        .transpose()?
        .flatten();
    let mut catalog = if cli.apply {
        let database_url = cli
            .database_url
            .as_deref()
            .context("--apply requires --database-url or OKF_PG_URL")?;
        Some(Catalog::connect(database_url, cli.tls, &tenant, &cli.bundle_name).await?)
    } else {
        None
    };
    let pages = client.crawl_space(&cli.space_id, cli.max_pages).await?;
    let documents = convert_pages(&source, &pages)?;
    let mut next = Snapshot::new(source, cli.bundle_name.clone(), tenant, &documents)?;
    if let Some(catalog) = &catalog {
        next.database_fingerprint = Some(catalog.fingerprint().into());
    }
    if let Some(previous) = &previous {
        next.check_previous(previous)?;
    }
    let removed = next.removed_ids(previous.as_ref());
    let warnings: Vec<_> = documents
        .iter()
        .filter(|doc| !doc.warnings.is_empty())
        .map(|doc| json!({ "page_id": doc.page_id, "warnings": doc.warnings }))
        .collect();
    let mut result = json!({
        "mode": if cli.apply { "apply" } else { "dry-run" },
        "source": next.source,
        "bundle_name": next.bundle_name,
        "tenant": next.tenant,
        "page_count": documents.len(),
        "deletion_baseline": if previous.is_some() { "checkpoint" } else { "none" },
        "removed_page_ids": removed,
        "deletions_require_approval": !removed.is_empty() && !cli.allow_deletions,
        "warnings": warnings,
    });
    if let Some(path) = &cli.output_dir {
        snapshot::export(path, &next, &documents)?;
    }
    if let Some(catalog) = &mut catalog {
        let report = catalog
            .apply(
                &mut next,
                previous.as_ref(),
                &documents,
                cli.allow_deletions,
            )
            .await?;
        snapshot::write_checkpoint(
            cli.state_file
                .as_deref()
                .context("--apply requires --state-file")?,
            &next,
        )?;
        result["catalog"] = serde_json::to_value(report)?;
    }
    println!("{}", serde_json::to_string_pretty(&result)?);
    Ok(())
}

fn confluence_client(cli: &Cli) -> Result<(Client, SourceIdentity)> {
    let token = load_token(
        cli.token_file.as_deref().map(Path::new),
        non_empty(std::env::var("CONFLUENCE_API_TOKEN").ok()),
    )?;
    let client = Client::new(ClientConfig {
        site_url: cli.site_url.clone(),
        email: cli.email.clone(),
        token,
        cloud_id: cli.cloud_id.clone(),
        max_retries: cli.max_retries,
        max_response_bytes: cli.max_response_bytes,
    })?;
    // Client::new validates HTTPS, host, credentials, query and path before this normalization.
    let canonical = url::Url::parse(&cli.site_url).context("invalid Confluence site URL")?;
    let source = SourceIdentity {
        site_url: canonical.origin().ascii_serialization(),
        cloud_id: cli.cloud_id.clone(),
        space_id: cli.space_id.clone(),
    };
    Ok((client, source))
}

/// Read the API token from exactly one source: a token file, or the
/// already-normalized `CONFLUENCE_API_TOKEN` value.
fn load_token(path: Option<&Path>, environment: Option<String>) -> Result<String> {
    ensure!(
        path.is_none() || environment.is_none(),
        "choose CONFLUENCE_API_TOKEN or a token file, not both"
    );
    let token = if let Some(path) = path {
        let metadata = std::fs::metadata(path).context("reading token file metadata")?;
        ensure!(
            metadata.is_file() && metadata.len() <= MAX_TOKEN_FILE_BYTES,
            "token file must be a regular file no larger than 64 KiB"
        );
        std::fs::read_to_string(path)
            .context("reading token file")?
            .trim_end_matches(['\r', '\n'])
            .to_owned()
    } else {
        environment.context("set CONFLUENCE_API_TOKEN or --token-file/CONFLUENCE_TOKEN_FILE")?
    };
    ensure!(
        !token.is_empty() && !token.chars().any(char::is_whitespace),
        "token must be nonempty and contain no whitespace"
    );
    Ok(token)
}

#[cfg(test)]
mod tests {
    use super::*;

    const REQUIRED: [&str; 9] = [
        "pgokf-confluence",
        "--site-url",
        "https://test.atlassian.net",
        "--email",
        "a@example.org",
        "--space-id",
        "1",
        "--bundle-name",
        "wiki",
    ];

    #[test]
    fn cli_defaults_to_a_read_only_dry_run() {
        // Arrange / Act
        let parsed = Cli::try_parse_from(REQUIRED).expect("required arguments parse");

        // Assert
        assert!(!parsed.apply);
        assert!(!parsed.allow_deletions);
    }

    #[test]
    fn cli_has_no_token_argument() {
        // Arrange
        let args = REQUIRED.into_iter().chain(["--token", "secret"]);

        // Act
        let parsed = Cli::try_parse_from(args);

        // Assert
        assert!(parsed.is_err());
    }

    #[test]
    fn cli_apply_requires_a_database_and_a_checkpoint() {
        // Arrange
        let args = REQUIRED.into_iter().chain(["--apply"]);

        // Act
        let parsed = Cli::try_parse_from(args);

        // Assert
        assert!(parsed.is_err());
    }

    #[test]
    fn cli_normalization_treats_empty_optional_values_as_unset() {
        // Arrange
        let args = REQUIRED.into_iter().chain([
            "--tenant",
            "",
            "--token-file",
            "",
            "--cloud-id",
            "",
            "--database-url",
            "",
        ]);

        // Act
        let cli = Cli::try_parse_from(args).expect("parses").normalized();

        // Assert
        assert!(cli.tenant.is_none());
        assert!(cli.token_file.is_none());
        assert!(cli.cloud_id.is_none());
        assert!(cli.database_url.is_none());
    }

    #[test]
    fn cli_normalization_lowercases_the_cloud_id() {
        // Arrange
        let args = REQUIRED
            .into_iter()
            .chain(["--cloud-id", "ABCDEF01-2345-6789-ABCD-EF0123456789"]);

        // Act
        let cli = Cli::try_parse_from(args).expect("parses").normalized();

        // Assert
        assert_eq!(
            cli.cloud_id.as_deref(),
            Some("abcdef01-2345-6789-abcd-ef0123456789")
        );
    }

    #[test]
    fn load_token_strips_one_trailing_line_ending_from_a_file() {
        // Arrange
        let file = tempfile::NamedTempFile::new().expect("temp file");
        std::fs::write(file.path(), b"test-token\r\n").expect("write token");

        // Act
        let token = load_token(Some(file.path()), None);

        // Assert
        assert_eq!(token.expect("token loads"), "test-token");
    }

    #[test]
    fn load_token_rejects_two_sources() {
        // Arrange
        let file = tempfile::NamedTempFile::new().expect("temp file");
        std::fs::write(file.path(), b"test-token\n").expect("write token");

        // Act
        let token = load_token(Some(file.path()), Some("other".into()));

        // Assert
        assert!(token.is_err());
    }

    #[test]
    fn load_token_rejects_embedded_whitespace() {
        // Arrange / Act
        let token = load_token(None, Some("bad\ntoken".into()));

        // Assert
        assert!(token.is_err());
    }

    #[test]
    fn load_token_requires_a_source() {
        // Arrange / Act
        let token = load_token(None, None);

        // Assert
        assert!(token.is_err());
    }
}
