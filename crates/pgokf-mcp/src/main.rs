// SPDX-License-Identifier: AGPL-3.0-only
//! `pgokf-mcp` - a Model Context Protocol server exposing the `pgokf` catalog.
//!
//! This standalone async binary speaks MCP over stdio: newline-delimited
//! JSON-RPC 2.0 on stdin/stdout. It implements the MCP handshake
//! (`initialize` → `serverInfo`/`capabilities`, then `tools/list` and
//! `tools/call`) and exposes the catalog's search and graph functions as MCP
//! tools, each backed by a query against the shipped `pgokf` public surface.
//!
//! The JSON-RPC layer is hand-rolled on `serde_json` (see `rpc.rs`) so the
//! server carries no heavy MCP SDK dependency. Wire it into any MCP client by
//! launching this binary as a stdio server (see the README).

// The prose names products (PostgreSQL, JSON-RPC, MCP, ...); backticking each
// occurrence would harm readability more than it helps.
#![allow(clippy::doc_markdown)]

mod catalog;
mod rpc;

use anyhow::{Context, Result};
use clap::Parser;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Stdout};

use crate::catalog::Catalog;
use crate::rpc::{INVALID_PARAMS, METHOD_NOT_FOUND, PARSE_ERROR, Request, Response};

/// The MCP protocol revision this server implements.
const PROTOCOL_VERSION: &str = "2024-11-05";

/// Command-line / environment configuration for the server.
#[derive(Debug, Parser)]
#[command(
    name = "pgokf-mcp",
    about = "Expose the pgokf catalog to AI agents as Model Context Protocol tools over stdio."
)]
struct Cli {
    /// PostgreSQL connection string for a `pgokf_reader`-capable role.
    #[arg(long, env = "OKF_PG_URL", hide_env_values = true)]
    database_url: String,

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
    let catalog = Catalog::connect(&cli.database_url, cli.tenant.as_deref(), cli.tls)
        .await
        .context("failed to connect to the catalog")?;
    serve(catalog).await
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
    let request: Request = match serde_json::from_str(line) {
        Ok(request) => request,
        Err(error) => {
            return Some(Response::error(
                Value::Null,
                PARSE_ERROR,
                format!("invalid JSON-RPC request: {error}"),
            ));
        }
    };

    if request.is_notification() {
        // Notifications (for example notifications/initialized) get no reply.
        return None;
    }

    let id = request.id.clone().unwrap_or(Value::Null);
    Some(dispatch(catalog, &request, id).await)
}

/// Route a request by method to its handler.
async fn dispatch(catalog: &Catalog, request: &Request, id: Value) -> Response {
    match request.method.as_str() {
        "initialize" => Response::success(id, initialize_result()),
        "tools/list" => Response::success(id, json!({ "tools": Catalog::tool_definitions() })),
        "tools/call" => tools_call(catalog, &request.params, id).await,
        "ping" => Response::success(id, json!({})),
        other => Response::error(id, METHOD_NOT_FOUND, format!("unknown method '{other}'")),
    }
}

/// The `initialize` result: protocol version, capabilities, and server info.
fn initialize_result() -> Value {
    json!({
        "protocolVersion": PROTOCOL_VERSION,
        "capabilities": { "tools": {} },
        "serverInfo": {
            "name": "pgokf-mcp",
            "version": env!("CARGO_PKG_VERSION"),
        }
    })
}

/// Handle `tools/call`: validate the envelope, run the tool, and wrap the
/// outcome as an MCP tool result. A tool-execution failure is reported in-band
/// as an `isError` result (not a JSON-RPC protocol error), per MCP.
async fn tools_call(catalog: &Catalog, params: &Value, id: Value) -> Response {
    let Some(name) = params.get("name").and_then(Value::as_str) else {
        return Response::error(id, INVALID_PARAMS, "tools/call requires a string 'name'");
    };
    let empty = json!({});
    let arguments = params.get("arguments").unwrap_or(&empty);

    match catalog.call_tool(name, arguments).await {
        Ok(data) => Response::success(id, tool_result(&data, false)),
        Err(error) => Response::success(id, tool_result(&json!(format!("{error:#}")), true)),
    }
}

/// Wrap tool output as an MCP tool result: a single text content block holding
/// the JSON, with the `isError` flag.
fn tool_result(data: &Value, is_error: bool) -> Value {
    let text = serde_json::to_string_pretty(data).unwrap_or_else(|_| "null".to_owned());
    json!({
        "content": [ { "type": "text", "text": text } ],
        "isError": is_error,
    })
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
