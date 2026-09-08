// SPDX-License-Identifier: AGPL-3.0-only
//! `pgokf-mcp` - a Model Context Protocol server exposing the `pgokf` catalog.
//!
//! This standalone async binary speaks MCP over stdio by default:
//! newline-delimited JSON-RPC 2.0 on stdin/stdout. It implements the MCP
//! handshake (`initialize` → `serverInfo`/`capabilities`, then `tools/list`
//! and `tools/call`) and exposes the catalog's search and graph functions as
//! MCP tools, each backed by a query against the shipped `pgokf` public
//! surface.
//!
//! With `--http` it serves the same messages over HTTP instead, for clients
//! that cannot launch a subprocess. That endpoint is reachable, so it is
//! never open: every request carries a bearer token, and the token's role
//! decides which tools it may call (see [`tokens`] and [`http`]).
//!
//! The JSON-RPC layer is hand-rolled on `serde_json` (see `rpc.rs`) so the
//! server carries no heavy MCP SDK dependency. Wire it into any MCP client by
//! launching this binary as a stdio server (see the README).

// The prose names products (PostgreSQL, JSON-RPC, MCP, ...); backticking each
// occurrence would harm readability more than it helps.
#![allow(clippy::doc_markdown)]

mod catalog;
mod dispatch;
mod http;
mod rpc;
mod tokens;

use std::collections::BTreeMap;
use std::net::SocketAddr;

use anyhow::{Context, Result};
use clap::Parser;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Stdout};

use crate::catalog::Catalog;
use crate::dispatch::Caller;
use crate::rpc::Response;

/// Command-line / environment configuration for the server.
#[derive(Debug, Parser)]
#[command(
    name = "pgokf-mcp",
    about = "Expose the pgokf catalog to AI agents as Model Context Protocol tools over stdio."
)]
struct Cli {
    /// Serve MCP over HTTP on this address instead of over stdio. The
    /// endpoint is `POST /mcp`; every request carries a bearer token minted
    /// on the catalog's Admin page (pgokf-web), and it expects TLS to be
    /// terminated in front of it. Bind it to the loopback interface unless
    /// something else guards it.
    #[arg(long, env = "OKF_MCP_HTTP_BIND", value_name = "ADDR")]
    http: Option<SocketAddr>,

    /// Browser origins allowed to call the HTTP endpoint, comma-separated.
    /// Empty (the default) refuses every request carrying an `Origin`,
    /// which is what keeps a page in someone's browser from reaching this
    /// server on their network.
    #[arg(long, env = "OKF_MCP_ALLOWED_ORIGINS", default_value = "")]
    allowed_origins: String,

    /// PostgreSQL connection string for a `pgokf_reader`-capable role. Also
    /// read as `OKF_PG_URL` from the `--env-file`, then from the environment.
    #[arg(long, hide_env_values = true)]
    database_url: Option<String>,

    /// Optional multi-tenant scope applied as `pgokf.tenant` for the session
    /// (`OKF_TENANT` in the env file or the environment).
    #[arg(long)]
    tenant: Option<String>,

    /// Require a TLS-encrypted link to PostgreSQL (`OKF_PG_TLS` in the env
    /// file or the environment). TLS is also enabled by an `sslmode=require`
    /// (or stricter) in the connection URL; otherwise the link is plaintext
    /// (the default, for a local socket / trusted network).
    #[arg(long, num_args = 0..=1, default_missing_value = "true", value_name = "BOOL")]
    tls: Option<bool>,

    /// A `KEY=VALUE` file read for the settings above (an Agent Plugins
    /// package points at `${PLUGIN_DATA}/pgokf.env`). A flag on the command
    /// line wins over the file, and the file wins over the environment, so
    /// an installed plugin always talks to the catalog its own file names.
    #[arg(long, value_name = "PATH")]
    env_file: Option<String>,
}

/// The resolved connection settings.
#[derive(Debug, PartialEq, Eq)]
struct Settings {
    database_url: String,
    tenant: Option<String>,
    tls: bool,
}

/// Apply the precedence: command line, then the env file, then the
/// process environment (`ambient`). Empty values count as unset.
fn resolve_settings(
    cli: &Cli,
    file: &BTreeMap<String, String>,
    ambient: &BTreeMap<String, String>,
) -> Result<Settings> {
    let pick = |flag: Option<&str>, key: &str| -> Option<String> {
        flag.map(str::to_owned)
            .or_else(|| file.get(key).cloned())
            .or_else(|| ambient.get(key).cloned())
            .filter(|v| !v.trim().is_empty())
    };
    let database_url = pick(cli.database_url.as_deref(), "OKF_PG_URL").context(
        "no connection string: pass --database-url, set OKF_PG_URL, or name an --env-file that sets it",
    )?;
    let tenant = pick(cli.tenant.as_deref(), "OKF_TENANT");
    let tls = match cli.tls {
        Some(flag) => flag,
        None => pick(None, "OKF_PG_TLS").is_some_and(|v| matches!(v.trim(), "1" | "true" | "on")),
    };
    Ok(Settings {
        database_url,
        tenant,
        tls,
    })
}

/// The variables this server consults, as the process environment holds
/// them.
fn ambient_settings() -> BTreeMap<String, String> {
    ["OKF_PG_URL", "OKF_TENANT", "OKF_PG_TLS"]
        .into_iter()
        .filter_map(|key| std::env::var(key).ok().map(|v| (key.to_owned(), v)))
        .collect()
}

/// Parse `KEY=VALUE` lines (blank lines and `#` comments skipped, an
/// `export ` prefix and surrounding quotes tolerated) into a map.
fn parse_env_file(text: &str) -> BTreeMap<String, String> {
    let mut vars = BTreeMap::new();
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let key = key.trim();
        let key = key.strip_prefix("export ").unwrap_or(key).trim();
        let value = value.trim().trim_matches('"').trim_matches('\'');
        if !key.is_empty() {
            vars.insert(key.to_owned(), value.to_owned());
        }
    }
    vars
}

/// The variables of the `--env-file`, when one is given. Used by Agent
/// Plugins packages, whose `mcp.json` points at `${PLUGIN_DATA}/pgokf.env`
/// (a file the user creates once outside the plugin, so no secret ever
/// enters the package). A value given on the command line or in the
/// environment wins over the file; a missing file is an error naming it.
fn env_file_vars(path: Option<&str>) -> Result<BTreeMap<String, String>> {
    let Some(path) = path else {
        return Ok(BTreeMap::new());
    };
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("reading the env file {path} (create it with OKF_PG_URL=...)"))?;
    Ok(parse_env_file(&text))
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let file = env_file_vars(cli.env_file.as_deref())?;
    let settings = resolve_settings(&cli, &file, &ambient_settings())?;
    let catalog = Catalog::connect(
        &settings.database_url,
        settings.tenant.as_deref(),
        settings.tls,
    )
    .await
    .context("failed to connect to the catalog")?;
    match cli.http {
        Some(bind) => {
            // Token lookups get a connection of their own (see `tokens`), on
            // the same reader URL, so authentication never waits behind work.
            let lookups =
                tokens::CatalogTokens::connect(&settings.database_url, settings.tls).await?;
            let server = http::Server::new(
                catalog,
                http::Admission::new(Box::new(lookups), settings.tenant.clone()),
                http::parse_origins(&cli.allowed_origins)?,
            );
            http::serve(server, bind).await
        }
        None => serve(catalog).await,
    }
}

/// Read newline-delimited JSON-RPC from stdin, dispatch each message, and write
/// each response as one line to stdout. Returns when stdin reaches EOF.
async fn serve(catalog: Catalog) -> Result<()> {
    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    let mut stdout = tokio::io::stdout();

    while let Some(line) = lines.next_line().await.context("reading from stdin")? {
        if line.trim().is_empty() {
            continue;
        }

        if let Some(response) = handle_line(&catalog, &line).await {
            write_response(&mut stdout, &response).await?;
        }
    }

    Ok(())
}

/// Parse and dispatch one input line, returning the response to send, or `None`
/// for a notification (which is never answered).
async fn handle_line(catalog: &Catalog, line: &str) -> Option<Response> {
    let request = match rpc::parse_request(line) {
        Ok(request) => request,
        Err(response) => return Some(response),
    };
    if request.is_notification() {
        // Notifications (for example notifications/initialized) get no reply.
        return None;
    }
    let id = request.reply_id();
    // The client started this process and holds the connection string:
    // there is nobody to authenticate and nothing to withhold.
    Some(dispatch::handle(catalog, &Caller::Local, &request, id).await)
}

/// Serialize a response to a single line and flush it to stdout.
async fn write_response(stdout: &mut Stdout, response: &Response) -> Result<()> {
    let mut line = serde_json::to_string(response).context("serializing the response")?;
    line.push('\n');
    stdout
        .write_all(line.as_bytes())
        .await
        .context("writing to stdout")?;
    stdout.flush().await.context("flushing stdout")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_env_file_reads_assignments_and_skips_noise() {
        // Arrange
        let text = "# comment\n\nexport OKF_PG_URL=\"postgresql://r@h/db\"\nOKF_TENANT='acme'\nbroken line\n  OKF_PG_TLS = true \n";

        // Act
        let vars = parse_env_file(text);

        // Assert
        assert_eq!(
            vars.get("OKF_PG_URL").map(String::as_str),
            Some("postgresql://r@h/db")
        );
        assert_eq!(vars.get("OKF_TENANT").map(String::as_str), Some("acme"));
        assert_eq!(vars.get("OKF_PG_TLS").map(String::as_str), Some("true"));
        assert_eq!(vars.len(), 3);
    }

    #[test]
    fn parse_env_file_ignores_a_byte_order_mark_and_keeps_equals_in_values() {
        // Arrange / Act
        let vars = parse_env_file("\u{feff}OKF_PG_URL=postgresql://u@h/db?options=-c%20a=b\r\n");

        // Assert
        assert_eq!(
            vars.get("OKF_PG_URL").map(String::as_str),
            Some("postgresql://u@h/db?options=-c%20a=b")
        );
    }

    /// A `Cli` with nothing set, so a test names only the fields it is about.
    fn bare_cli() -> Cli {
        Cli {
            http: None,
            allowed_origins: String::new(),
            database_url: None,
            tenant: None,
            tls: None,
            env_file: None,
        }
    }

    #[test]
    fn settings_prefer_the_flag_then_the_file_then_the_environment() {
        // Arrange
        let file = BTreeMap::from([
            ("OKF_PG_URL".to_owned(), "postgresql://file".to_owned()),
            ("OKF_PG_TLS".to_owned(), "on".to_owned()),
        ]);
        let ambient = BTreeMap::from([
            ("OKF_PG_URL".to_owned(), "postgresql://ambient".to_owned()),
            ("OKF_TENANT".to_owned(), "acme".to_owned()),
            ("OKF_PG_TLS".to_owned(), "false".to_owned()),
        ]);
        let base = bare_cli();

        // Act
        let from_file = resolve_settings(&base, &file, &ambient).expect("resolves");
        let flagged = resolve_settings(
            &Cli {
                database_url: Some("postgresql://flag".to_owned()),
                tls: Some(false),
                ..base
            },
            &file,
            &ambient,
        )
        .expect("resolves");
        let nothing = resolve_settings(&bare_cli(), &BTreeMap::new(), &BTreeMap::new());

        // Assert
        assert_eq!(
            from_file,
            Settings {
                database_url: "postgresql://file".to_owned(),
                tenant: Some("acme".to_owned()),
                tls: true,
            }
        );
        assert_eq!(flagged.database_url, "postgresql://flag");
        assert!(!flagged.tls, "an explicit --tls false beats the file");
        assert!(
            nothing
                .expect_err("no url")
                .to_string()
                .contains("no connection string")
        );
    }

    #[test]
    fn a_missing_env_file_is_an_error_naming_it() {
        // Arrange / Act
        let error = env_file_vars(Some("/nonexistent/pgokf.env")).expect_err("missing file");

        // Assert
        assert!(error.to_string().contains("/nonexistent/pgokf.env"));
        assert!(env_file_vars(None).expect("no file is fine").is_empty());
    }
}
