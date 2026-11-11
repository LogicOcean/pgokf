// SPDX-License-Identifier: AGPL-3.0-only
//! Shared PostgreSQL connection helper for the `pgokf` companions.
//!
//! The three companions (`pgokf-ingest`, `pgokf-embed`, `pgokf-mcp`) all open a
//! single `tokio-postgres` connection and drive its protocol future on a
//! background task. Historically each hard-coded `NoTls`, so an operator who
//! required an encrypted link (`sslmode=require`) could not connect at all. This
//! crate centralizes the connect step, adds **optional** TLS, and owns the one
//! tenant-scoping step every companion applies ([`set_tenant`]):
//!
//! - **`NoTls` stays the default** - a local Unix-socket or trusted-network
//!   deployment is unchanged and links in plaintext, negotiating no TLS.
//! - TLS is negotiated only when the operator asks for it: an `sslmode=require`
//!   in the connection string, or an explicit `--tls` flag threaded in as
//!   `force_tls`. The link then uses `rustls` with the platform trust store to
//!   verify the server certificate chain and host name.
//! - **`--tls` is a requirement, not a preference.** [`connection_config`]
//!   raises a weaker `sslmode` (`disable`, or the default `prefer`) to
//!   `require`, so a server that declines TLS is refused before any startup
//!   message - user name, database, or password exchange - crosses the wire.
//!
//! The crate reuses the `rustls` stack that `object_store -> reqwest` already
//! pulls into the workspace, so it introduces no new cryptographic dependency
//! and `cargo deny` stays green.

// The prose names products (PostgreSQL, TLS, rustls, ...); backticking each
// occurrence would harm readability more than it helps.
#![allow(clippy::doc_markdown)]

use std::sync::Arc;

use anyhow::{Context, Result, bail};
use tokio::task::JoinHandle;
use tokio_postgres::config::SslMode;
use tokio_postgres::tls::{MakeTlsConnect, TlsConnect};
use tokio_postgres::{Client, Config, NoTls, Socket};
use tokio_postgres_rustls::MakeRustlsConnect;

/// Scope the session to one pgokf tenant: sets the `pgokf.tenant` GUC for the
/// connection (session-wide, not transaction-local), so every later catalog
/// call sees and touches only that tenant's rows through row-level security,
/// and satisfies a catalog whose `require_tenant` policy is on. Every
/// companion applies its `--tenant` / `OKF_TENANT` through this one helper.
///
/// # Errors
///
/// Returns an error if the `set_config` call fails.
pub async fn set_tenant(client: &Client, tenant: &str) -> Result<()> {
    client
        .execute(
            "SELECT pg_catalog.set_config('pgokf.tenant', $1, false)",
            &[&tenant],
        )
        .await
        .context("failed to set pgokf.tenant")?;
    Ok(())
}

/// Connect to PostgreSQL, returning the [`Client`] and the [`JoinHandle`] of the
/// background task driving the connection's protocol future.
///
/// TLS is required when `force_tls` is set (the companion's `--tls` flag) or
/// the connection string's `sslmode` is `require`; otherwise the link uses
/// `NoTls` (the default), exactly as before. See [`connection_config`] for why
/// forcing TLS also rewrites the `sslmode`. The caller keeps the handle to
/// `.await` (or drop) after it drops the client, so any transport error is
/// surfaced on shutdown.
///
/// # Errors
///
/// Returns an error if the connection string cannot be parsed, the TLS trust
/// store cannot be built, or the connection cannot be established - including
/// when TLS is required and the server declines it.
pub async fn connect(database_url: &str, force_tls: bool) -> Result<(Client, JoinHandle<()>)> {
    let config = connection_config(database_url, force_tls)?;

    if should_use_tls(config.get_ssl_mode(), force_tls) {
        let tls = rustls_connector()?;
        spawn_connection(&config, tls).await
    } else {
        spawn_connection(&config, NoTls).await
    }
}

/// Parse a libpq-style connection string into a `tokio-postgres` [`Config`],
/// for callers that build their own connection pool on the same settings.
///
/// # Errors
///
/// Returns an error when the string is not a valid connection string.
pub fn parse_config(database_url: &str) -> Result<Config> {
    database_url
        .parse()
        .context("parsing the PostgreSQL connection string")
}

/// Parse a connection string and apply the companion's TLS policy: the
/// [`Config`] every connection - single or pooled - must be built from.
///
/// With `force_tls`, an `sslmode` weaker than `require` is raised to it.
/// Choosing a TLS connector is not enough on its own: under `prefer` (the
/// libpq default when no `sslmode` is given) `tokio-postgres` falls back to
/// plaintext when the server answers the TLS request with "no", and under
/// `disable` it never asks. Either would send the startup message and
/// credentials in clear text despite an explicit `--tls`. Without
/// `force_tls`, the string's own `sslmode` stands unchanged.
///
/// # Errors
///
/// Returns an error when the string is not a valid connection string.
pub fn connection_config(database_url: &str, force_tls: bool) -> Result<Config> {
    let mut config = parse_config(database_url)?;
    if force_tls && matches!(config.get_ssl_mode(), SslMode::Disable | SslMode::Prefer) {
        config.ssl_mode(SslMode::Require);
    }
    Ok(config)
}

/// The rustls-backed TLS connector [`connect`] uses, for callers that build a
/// pool and need the same trust configuration (native roots, aws-lc-rs).
///
/// # Errors
///
/// Returns an error when the native root store cannot be loaded.
pub fn rustls_connector() -> Result<MakeRustlsConnect> {
    build_rustls_connector().context("building the PostgreSQL TLS connector")
}

/// Decide whether to negotiate TLS for this connection.
///
/// `NoTls` is the default so a local-socket / trusted-network deployment is
/// unchanged: TLS is used only when the operator opts in with the `--tls` flag
/// (`force_tls`) or an `sslmode` that demands encryption. `disable` and `prefer`
/// keep the plaintext default; `require` and the stricter verify modes - and any
/// future non-default mode - negotiate TLS.
#[must_use]
pub fn should_use_tls(ssl_mode: SslMode, force_tls: bool) -> bool {
    force_tls || !matches!(ssl_mode, SslMode::Disable | SslMode::Prefer)
}

/// Connect with a chosen TLS strategy and spawn the connection driver task.
///
/// Generic over the `tokio-postgres` TLS strategy so the `NoTls` and `rustls`
/// paths share one implementation (the connection-driver spawn is identical);
/// the bounds are the standard set `tokio_postgres::Config::connect` requires of
/// a `MakeTlsConnect`.
async fn spawn_connection<T>(config: &Config, tls: T) -> Result<(Client, JoinHandle<()>)>
where
    T: MakeTlsConnect<Socket>,
    T::Stream: Send + 'static,
    T::TlsConnect: Send,
    <T::TlsConnect as TlsConnect<Socket>>::Future: Send,
{
    let (client, connection) = config
        .connect(tls)
        .await
        .context("connecting to PostgreSQL")?;
    let handle = tokio::spawn(async move {
        if let Err(error) = connection.await {
            eprintln!("pgokf: PostgreSQL connection error: {error}");
        }
    });
    Ok((client, handle))
}

/// Build a `rustls` TLS connector that verifies the server certificate against
/// the platform trust store.
///
/// The `aws_lc_rs` crypto provider (rustls' default) is selected explicitly so
/// no process-wide default provider needs to be installed - avoiding a global
/// mutation and the ambiguity of the tree carrying more than one provider.
fn build_rustls_connector() -> Result<MakeRustlsConnect> {
    let mut roots = rustls::RootCertStore::empty();
    let native = rustls_native_certs::load_native_certs();
    for certificate in native.certs {
        // Skip an individual malformed platform certificate rather than failing
        // the whole connection; the remaining roots still anchor verification.
        let _ = roots.add(certificate);
    }
    if roots.is_empty() {
        bail!(
            "no native root certificates were found to verify the PostgreSQL server certificate; \
             install the system CA bundle or connect without sslmode=require"
        );
    }

    let config = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::aws_lc_rs::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .context("selecting TLS protocol versions")?
    .with_root_certificates(roots)
    .with_no_client_auth();

    Ok(MakeRustlsConnect::new(config))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_use_tls_defaults_to_plaintext() {
        // Arrange / Act / Assert: without the flag, the plaintext-default modes
        // stay on NoTls (the local-socket default is preserved).
        assert!(!should_use_tls(SslMode::Disable, false));
        assert!(!should_use_tls(SslMode::Prefer, false));
    }

    #[test]
    fn should_use_tls_honors_sslmode_require() {
        // Arrange / Act / Assert: sslmode=require opts the link into TLS with no
        // flag needed.
        assert!(should_use_tls(SslMode::Require, false));
    }

    #[test]
    fn should_use_tls_honors_the_force_flag_over_any_mode() {
        // Arrange / Act / Assert: an explicit --tls flag forces TLS even for a
        // plaintext-default sslmode.
        assert!(should_use_tls(SslMode::Disable, true));
        assert!(should_use_tls(SslMode::Prefer, true));
    }

    #[test]
    fn connection_config_with_tls_requires_encryption_for_every_weaker_mode() {
        // Arrange: no sslmode (libpq's prefer default), and both explicit
        // modes that allow a plaintext session.
        let urls = [
            "postgresql://writer@localhost/catalog",
            "postgresql://writer@localhost/catalog?sslmode=prefer",
            "postgresql://writer@localhost/catalog?sslmode=disable",
        ];

        for url in urls {
            // Act
            let config = connection_config(url, true).expect("valid connection string");

            // Assert
            assert_eq!(config.get_ssl_mode(), SslMode::Require, "{url}");
        }
    }

    #[test]
    fn connection_config_without_tls_keeps_the_strings_own_mode() {
        // Arrange
        let cases = [
            ("postgresql://writer@localhost/catalog", SslMode::Prefer),
            (
                "postgresql://writer@localhost/catalog?sslmode=disable",
                SslMode::Disable,
            ),
            (
                "postgresql://writer@localhost/catalog?sslmode=require",
                SslMode::Require,
            ),
        ];

        for (url, expected) in cases {
            // Act
            let config = connection_config(url, false).expect("valid connection string");

            // Assert
            assert_eq!(config.get_ssl_mode(), expected, "{url}");
        }
    }

    #[test]
    fn connection_config_rejects_an_invalid_string() {
        // Arrange
        let url = "postgresql://writer@localhost/catalog?sslmode=sometimes";

        // Act
        let result = connection_config(url, true);

        // Assert
        assert!(result.is_err());
    }

    /// The PostgreSQL `SSLRequest` packet: length 8, request code 80877103.
    const SSL_REQUEST: [u8; 8] = [0, 0, 0, 8, 4, 210, 22, 47];

    /// Accept one connection, decline its TLS request the way a server without
    /// TLS does (a single `N`), and report whether the client then sent
    /// anything - a startup message would mean a plaintext fallback.
    async fn decline_tls_once(listener: tokio::net::TcpListener) -> bool {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let (mut stream, _) = listener.accept().await.expect("client connects");
        let mut request = [0_u8; 8];
        stream
            .read_exact(&mut request)
            .await
            .expect("client sends a request");
        assert_eq!(request, SSL_REQUEST, "first packet must be SSLRequest");
        stream.write_all(b"N").await.expect("decline TLS");
        let mut next = [0_u8; 1];
        let sent_more = stream.read(&mut next).await.expect("read after decline");
        sent_more > 0
    }

    #[tokio::test]
    async fn connect_with_tls_refuses_a_server_that_declines_encryption() {
        for mode in ["prefer", "disable"] {
            // Arrange: a local endpoint that refuses TLS, reached with a mode
            // that would otherwise permit a plaintext session.
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
                .await
                .expect("bind a local port");
            let port = listener.local_addr().expect("bound address").port();
            let server = tokio::spawn(decline_tls_once(listener));
            let url = format!(
                "host=localhost hostaddr=127.0.0.1 port={port} user=writer dbname=catalog \
                 sslmode={mode} connect_timeout=5"
            );

            // Act
            let result = connect(&url, true).await;

            // Assert: the connection fails and nothing followed the declined
            // TLS request, so no credentials or startup data were exposed.
            assert!(result.is_err(), "sslmode={mode} must not fall back");
            let sent_plaintext = tokio::time::timeout(std::time::Duration::from_secs(5), server)
                .await
                .expect("server observes the client closing")
                .expect("server task completes");
            assert!(!sent_plaintext, "sslmode={mode} sent a plaintext startup");
        }
    }
}
