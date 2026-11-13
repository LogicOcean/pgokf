// SPDX-License-Identifier: AGPL-3.0-only
//! `pgokf-confluence-events` - receive Confluence events from the Forge app.
//!
//! The Forge app in `integrations/confluence-forge` has Confluence Cloud push
//! page, comment, and task events to this HTTP endpoint through Forge
//! Remote. Each delivery must carry a Forge Invocation Token that verifies
//! against Atlassian's published keys and names the configured Forge app and
//! Confluence site; anything else is refused. Accepted events are written to
//! standard output as JSON lines (what changed, on which page, by whom, and a
//! key that is identical for redeliveries) - never a token.
//!
//! The server speaks plain HTTP. Forge reaches it only over the internet
//! with HTTPS, so put it behind a TLS-terminating proxy or ingress.

// CLI doc comments render as plain help text, and the prose names products
// (Confluence, Forge, Atlassian, ...); backticking each occurrence would harm
// readability, so the pedantic doc-markdown lint is relaxed here.
#![allow(clippy::doc_markdown)]

use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::{Context, Result};
use clap::Parser;
use pgokf_confluence::forge::fit::{FORGE_JWKS_URL, FitVerifier, ForgeKeys, Trust};
use pgokf_confluence::forge::receiver::{EVENTS_PATH, JsonLinesSink, Receiver, router};

/// Receive Confluence page, comment, and task events pushed by the pgokf Forge app.
#[derive(Parser)]
#[command(
    name = "pgokf-confluence-events",
    version,
    about = "Receive Confluence Cloud events pushed by the pgokf Forge app and write them as JSON lines."
)]
struct Cli {
    /// The Forge app ID, exactly as `forge register` wrote it into manifest.yml
    /// (ari:cloud:ecosystem::app/<uuid>). Deliveries for any other app are refused.
    #[arg(long, env = "FORGE_APP_ID")]
    forge_app_id: String,
    /// The Confluence site's cloud ID (a UUID). Deliveries from installations on
    /// any other site are refused.
    #[arg(long, env = "CONFLUENCE_CLOUD_ID")]
    cloud_id: String,
    /// Address to listen on. The server speaks plain HTTP; put a TLS-terminating
    /// proxy in front of it.
    #[arg(
        long,
        env = "PGOKF_CONFLUENCE_EVENTS_LISTEN",
        default_value = "127.0.0.1:8090"
    )]
    listen: SocketAddr,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let trust = Trust::new(&cli.forge_app_id, &cli.cloud_id)?;
    let verifier = FitVerifier::new(trust, ForgeKeys::new(FORGE_JWKS_URL)?);
    let receiver = Arc::new(Receiver::new(
        verifier,
        JsonLinesSink::new(std::io::stdout()),
    ));
    let listener = tokio::net::TcpListener::bind(cli.listen)
        .await
        .with_context(|| format!("binding {}", cli.listen))?;
    // The address actually bound: for port 0 the system picks the port, and
    // an announcement of the requested address would name none.
    let bound = listener.local_addr().context("reading the bound address")?;
    eprintln!("pgokf-confluence-events: receiving Forge deliveries on http://{bound}{EVENTS_PATH}");
    if !bound.ip().is_loopback() {
        eprintln!(
            "pgokf-confluence-events: {bound} is reachable beyond this host and this server speaks \
             plain HTTP; put a TLS-terminating proxy in front of it"
        );
    }
    let shutdown = pgokf_companion::daemon::shutdown_signal()?;
    axum::serve(listener, router(receiver))
        .with_graceful_shutdown(async move {
            if let Err(error) = shutdown.await {
                eprintln!("pgokf-confluence-events: shutdown signal error: {error}");
            }
        })
        .await
        .context("serving HTTP")
}

#[cfg(test)]
mod tests {
    use super::*;

    const APP_ID: &str = "ari:cloud:ecosystem::app/44444444-4444-4444-8444-444444444444";
    const CLOUD_ID: &str = "33333333-3333-4333-8333-333333333333";

    #[test]
    fn cli_defaults_to_a_loopback_listener() {
        // Arrange
        let args = [
            "pgokf-confluence-events",
            "--forge-app-id",
            APP_ID,
            "--cloud-id",
            CLOUD_ID,
        ];

        // Act
        let cli = Cli::try_parse_from(args).unwrap();

        // Assert
        assert!(cli.listen.ip().is_loopback());
        assert_eq!(cli.listen.port(), 8090);
    }

    #[test]
    fn cli_requires_the_app_and_the_site() {
        // Arrange
        let args = ["pgokf-confluence-events", "--forge-app-id", APP_ID];

        // Act
        let result = Cli::try_parse_from(args);

        // Assert
        assert!(result.is_err());
    }
}
