// SPDX-License-Identifier: AGPL-3.0-only
//! Shared PostgreSQL connection helper for the `pgokf` companions.
//!
//! The three companions (`pgokf-ingest`, `pgokf-embed`, `pgokf-mcp`) all open a
//! single `tokio-postgres` connection and drive its protocol future on a
//! background task. Historically each hard-coded `NoTls`, so an operator who
//! required an encrypted link (`sslmode=require`) could not connect at all. This
//! crate centralizes the connect step and adds **optional** TLS:
//!
//! - **`NoTls` stays the default** — a local Unix-socket or trusted-network
//!   deployment is unchanged and links in plaintext, negotiating no TLS.
//! - TLS is negotiated only when the operator asks for it: an `sslmode=require`
//!   (or a stricter `verify-ca` / `verify-full`) in the connection string, or an
//!   explicit `--tls` flag threaded in as `force_tls`. The link then uses
//!   `rustls` with the platform trust store to verify the server certificate.
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

/// Connect to PostgreSQL, returning the [`Client`] and the [`JoinHandle`] of the
/// background task driving the connection's protocol future.
///
/// TLS is negotiated when `force_tls` is set (the companion's `--tls` flag) or
/// the connection string's `sslmode` is `require` / `verify-ca` / `verify-full`;
/// otherwise the link uses `NoTls` (the default), exactly as before. The caller
/// keeps the handle to `.await` (or drop) after it drops the client, so any
/// transport error is surfaced on shutdown.
///
/// # Errors
///
/// Returns an error if the connection string cannot be parsed, the TLS trust
/// store cannot be built, or the connection cannot be established.
pub async fn connect(database_url: &str, force_tls: bool) -> Result<(Client, JoinHandle<()>)> {
    let config: Config = database_url
        .parse()
        .context("parsing the PostgreSQL connection string")?;

    if should_use_tls(config.get_ssl_mode(), force_tls) {
        let tls = build_rustls_connector().context("building the PostgreSQL TLS connector")?;
        spawn_connection(&config, tls).await
    } else {
        spawn_connection(&config, NoTls).await
    }
}

/// Decide whether to negotiate TLS for this connection.
///
/// `NoTls` is the default so a local-socket / trusted-network deployment is
/// unchanged: TLS is used only when the operator opts in with the `--tls` flag
/// (`force_tls`) or an `sslmode` that demands encryption. `disable` and `prefer`
/// keep the plaintext default; `require` and the stricter verify modes — and any
/// future non-default mode — negotiate TLS.
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
/// no process-wide default provider needs to be installed — avoiding a global
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
}
