// SPDX-License-Identifier: AGPL-3.0-only
//! One MCP implementation; the transports only frame it.
//!
//! `initialize`, `tools/list`, `tools/call` and `ping` are answered here for
//! stdio and for HTTP alike, so the two can never drift apart. The single
//! difference between them is *who is asking*, which is [`Caller`]: over
//! stdio the client started this process and already holds the connection
//! string, so there is nothing to authenticate and nothing to withhold;
//! over HTTP the caller is a bearer token whose role decides which tools it
//! may reach, and whose requests must never act on this host's filesystem.

use std::sync::OnceLock;

use serde_json::{Value, json};

use crate::catalog::{self, Catalog};
use crate::rpc::{INVALID_PARAMS, METHOD_NOT_FOUND, Request, Response};
use crate::tokens::{Bearer, Role, ToolAccess as _};

/// The MCP protocol revisions this server implements, oldest first. The
/// messages are the same across all three for the tools surface; the last
/// is what a client that asks for something else is answered with.
pub(crate) const PROTOCOL_VERSIONS: &[&str] = &["2024-11-05", "2025-03-26", "2025-06-18"];

/// JSON-RPC error code for a call the caller's role does not allow. The
/// specification reserves -32000 to -32099 for the server's own errors.
pub(crate) const NOT_PERMITTED: i64 = -32001;

/// How much of a caller-supplied name is quoted back into a log line or an
/// error message. The rest is the caller's to choose, so it is bounded and
/// escaped rather than echoed.
const ECHO_MAX: usize = 64;

/// Who is asking, and what that lets them reach.
pub(crate) enum Caller {
    /// The client launched this process and holds the connection string:
    /// nothing to authenticate, nothing to withhold, and writing a built
    /// tree into a directory writes it on the client's own machine.
    Local,
    /// A bearer token over the network: role-scoped, and never acting on
    /// this host.
    Remote(Bearer),
}

impl Caller {
    /// The tools this caller may call, from the one list both transports
    /// serve. A remote caller is never shown a tool its role cannot call,
    /// nor an argument that would act on this host.
    fn tools(&self) -> Value {
        let Value::Array(tools) = Catalog::tool_definitions() else {
            return json!([]);
        };
        let Caller::Remote(bearer) = self else {
            return Value::Array(tools);
        };
        Value::Array(
            tools
                .into_iter()
                .filter(|tool| {
                    tool["name"]
                        .as_str()
                        .is_some_and(|name| bearer.role.allows_tool(name))
                })
                .map(without_host_only_arguments)
                .collect(),
        )
    }

    /// Why this caller may not make this call, if it may not: the JSON-RPC
    /// code and the message to send back.
    fn refuse(&self, tool: &str, arguments: &Value) -> Option<(i64, String)> {
        let Caller::Remote(bearer) = self else {
            return None;
        };
        // A tool no role names is unknown rather than forbidden; the
        // catalog reports it, exactly as it does over stdio.
        if Role::any_allows(tool) && !bearer.role.allows_tool(tool) {
            let least = Role::all()
                .iter()
                .find(|role| role.allows_tool(tool))
                .map_or_else(|| "another".to_owned(), ToString::to_string);
            return Some((
                NOT_PERMITTED,
                format!(
                    "the {} role may not call {}; a token with the {least} role can",
                    bearer.role,
                    echo(tool)
                ),
            ));
        }
        let refused = host_only_arguments()
            .iter()
            .find(|name| arguments.get(name.as_str()).is_some_and(|v| !v.is_null()))?;
        Some((
            INVALID_PARAMS,
            format!(
                "'{refused}' acts on this server's own filesystem, not yours, and is refused \
                 over HTTP; leave it out and the files come back in the result"
            ),
        ))
    }

    /// How this caller is named in the log. Stdio is silent — its output is
    /// the client's — so this is only ever a remote one.
    fn note(&self, happened: &str, tool: &str) {
        if let Caller::Remote(bearer) = self {
            eprintln!(
                "pgokf-mcp: {} ({}) {happened} {}",
                bearer.name,
                bearer.role,
                echo(tool)
            );
        }
    }
}

/// Route one request by method. The `id` is the one to echo back.
pub(crate) async fn handle(
    catalog: &Catalog,
    caller: &Caller,
    request: &Request,
    id: Value,
) -> Response {
    match request.method.as_str() {
        "initialize" => Response::success(id, initialize_result(&request.params)),
        "tools/list" => Response::success(id, json!({ "tools": caller.tools() })),
        "tools/call" => tools_call(catalog, caller, &request.params, id).await,
        "ping" => Response::success(id, json!({})),
        other => Response::error(
            id,
            METHOD_NOT_FOUND,
            format!("unknown method {}", echo(other)),
        ),
    }
}

/// The `initialize` result: protocol version, capabilities, and server info.
///
/// The client names the revision it wants; it is echoed when this server
/// speaks it, and otherwise answered with the newest one this server does,
/// which is what the specification asks for.
pub(crate) fn initialize_result(params: &Value) -> Value {
    let asked = params.get("protocolVersion").and_then(Value::as_str);
    let version = asked
        .filter(|asked| PROTOCOL_VERSIONS.contains(asked))
        .or_else(|| PROTOCOL_VERSIONS.last().copied())
        .unwrap_or_default();
    json!({
        "protocolVersion": version,
        "capabilities": { "tools": {} },
        "serverInfo": {
            "name": "pgokf-mcp",
            "version": env!("CARGO_PKG_VERSION"),
        }
    })
}

/// Handle `tools/call`: check who is asking, validate the envelope, run the
/// tool, and wrap the outcome as an MCP tool result. A tool-execution
/// failure is reported in-band as an `isError` result (not a JSON-RPC
/// protocol error), per MCP.
async fn tools_call(catalog: &Catalog, caller: &Caller, params: &Value, id: Value) -> Response {
    let Some(name) = params.get("name").and_then(Value::as_str) else {
        return Response::error(id, INVALID_PARAMS, "tools/call requires a string 'name'");
    };
    let empty = json!({});
    let arguments = params.get("arguments").unwrap_or(&empty);

    if let Some((code, message)) = caller.refuse(name, arguments) {
        caller.note("was refused", name);
        return Response::error(id, code, message);
    }
    caller.note("called", name);

    match catalog.call_tool(name, arguments).await {
        Ok(data) => Response::success(id, tool_result(&data, false)),
        Err(error) => Response::success(id, tool_result(&json!(format!("{error:#}")), true)),
    }
}

/// Wrap tool output as an MCP tool result: a single text content block
/// holding the JSON, with the `isError` flag.
pub(crate) fn tool_result(data: &Value, is_error: bool) -> Value {
    let text = serde_json::to_string_pretty(data).unwrap_or_else(|_| "null".to_owned());
    json!({
        "content": [ { "type": "text", "text": text } ],
        "isError": is_error,
    })
}

/// The arguments the tool schemas mark as acting on the host this server
/// runs on. Reading them from the schemas rather than keeping a list here
/// means the next such argument is refused over the network the day it is
/// added, not the day someone remembers this file.
fn host_only_arguments() -> &'static [String] {
    static NAMES: OnceLock<Vec<String>> = OnceLock::new();
    NAMES.get_or_init(|| {
        let mut names: Vec<String> = Vec::new();
        let Value::Array(tools) = Catalog::tool_definitions() else {
            return names;
        };
        for tool in tools {
            let Some(properties) = tool
                .pointer("/inputSchema/properties")
                .and_then(Value::as_object)
            else {
                continue;
            };
            for (name, schema) in properties {
                if schema.get(catalog::HOST_ONLY).and_then(Value::as_bool) == Some(true)
                    && !names.contains(name)
                {
                    names.push(name.clone());
                }
            }
        }
        names
    })
}

/// A tool definition with the arguments that act on this host taken out, so
/// a remote client is never offered one it will be refused for using.
fn without_host_only_arguments(mut tool: Value) -> Value {
    if let Some(properties) = tool
        .pointer_mut("/inputSchema/properties")
        .and_then(Value::as_object_mut)
    {
        properties.retain(|_, schema| {
            schema.get(catalog::HOST_ONLY).and_then(Value::as_bool) != Some(true)
        });
    }
    tool
}

/// Quote a caller-supplied name back, bounded and escaped. It reaches a log
/// line and an error message, and everything in it was chosen by whoever
/// sent the request.
fn echo(text: &str) -> String {
    let mut shown: String = text.chars().take(ECHO_MAX).collect();
    if text.chars().nth(ECHO_MAX).is_some() {
        shown.push('…');
    }
    format!("{shown:?}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(tools: &Value) -> Vec<String> {
        tools
            .as_array()
            .expect("array")
            .iter()
            .filter_map(|tool| tool["name"].as_str().map(str::to_owned))
            .collect()
    }

    fn remote(role: Role) -> Caller {
        Caller::Remote(Bearer {
            tenant: None,
            name: "fleet".to_owned(),
            role,
        })
    }

    #[test]
    fn every_tool_the_catalog_defines_is_named_by_some_role() {
        // Arrange: the role table is a second list of the same tool names,
        // and a tool missing from it would work over stdio and be silently
        // unreachable over HTTP.
        let defined = names(&Catalog::tool_definitions());

        // Act / Assert
        assert!(!defined.is_empty());
        for name in defined {
            assert!(Role::any_allows(&name), "no role may call {name}");
        }
    }

    #[test]
    fn a_client_is_offered_only_the_tools_its_role_may_call() {
        // Arrange / Act
        let local = names(&Caller::Local.tools());
        let reader = names(&remote(Role::Reader).tools());
        let builder = names(&remote(Role::Builder).tools());

        // Assert
        assert!(reader.contains(&"concept_search".to_owned()));
        assert!(!reader.contains(&"build_workspace_plugin".to_owned()));
        assert!(builder.contains(&"build_workspace_plugin".to_owned()));
        assert!(builder.len() > reader.len());
        assert_eq!(local, builder, "stdio sees every tool a builder does");
    }

    #[test]
    fn a_remote_client_is_not_offered_an_argument_that_acts_on_this_host() {
        // Arrange
        let host_only = host_only_arguments();
        let builder = remote(Role::Builder).tools();
        let local = Caller::Local.tools();
        let arguments = |tools: &Value| -> Vec<String> {
            tools
                .as_array()
                .expect("array")
                .iter()
                .find(|tool| tool["name"] == "build_workspace_plugin")
                .and_then(|tool| tool["inputSchema"]["properties"].as_object())
                .map(|properties| properties.keys().cloned().collect())
                .unwrap_or_default()
        };

        // Act / Assert
        assert_eq!(host_only, ["output_dir", "overwrite"]);
        let offered = arguments(&builder);
        assert!(!offered.is_empty(), "the plugin builder has arguments");
        for name in host_only {
            assert!(!offered.contains(name), "{name} is offered remotely");
            assert!(arguments(&local).contains(name), "{name} is local-only");
        }
    }

    #[test]
    fn a_role_that_may_not_call_a_tool_is_told_which_one_can() {
        // Arrange / Act
        let refusal = remote(Role::Reader).refuse("build_workspace_plugin", &json!({}));

        // Assert
        let (code, message) = refusal.expect("refused");
        assert_eq!(code, NOT_PERMITTED);
        assert!(
            message.contains("the reader role may not call"),
            "{message}"
        );
        assert!(message.contains("the builder role can"), "{message}");
    }

    #[test]
    fn a_tool_nobody_has_heard_of_is_not_reported_as_a_role_refusal() {
        // Arrange / Act: it is unknown, not forbidden, and the catalog says
        // so on both transports.
        let reader = remote(Role::Reader).refuse("drop_everything", &json!({}));
        let builder = remote(Role::Builder).refuse("drop_everything", &json!({}));

        // Assert
        assert!(reader.is_none() && builder.is_none());
    }

    #[test]
    fn an_argument_that_would_write_on_this_host_is_named_and_refused() {
        // Arrange
        let builder = remote(Role::Builder);

        // Act
        let with_dir = builder.refuse(
            "build_workspace_plugin",
            &json!({ "target": "claude-code", "output_dir": "/etc" }),
        );
        let with_overwrite =
            builder.refuse("build_workspace_plugin", &json!({ "overwrite": true }));
        let plain = builder.refuse(
            "build_workspace_plugin",
            &json!({ "target": "claude-code" }),
        );
        let null = builder.refuse("build_workspace_plugin", &json!({ "output_dir": null }));
        let locally = Caller::Local.refuse(
            "build_workspace_plugin",
            &json!({ "output_dir": "/tmp/here" }),
        );

        // Assert
        assert!(
            with_dir
                .is_some_and(|(code, why)| code == INVALID_PARAMS && why.contains("'output_dir'"))
        );
        assert!(with_overwrite.is_some_and(|(_, why)| why.contains("'overwrite'")));
        assert!(plain.is_none() && null.is_none());
        assert!(
            locally.is_none(),
            "stdio writes on the client's own machine"
        );
    }

    #[test]
    fn initialize_answers_with_the_revision_the_client_asked_for_when_it_can() {
        // Arrange / Act
        let asked = initialize_result(&json!({ "protocolVersion": "2024-11-05" }));
        let newer = initialize_result(&json!({ "protocolVersion": "2025-06-18" }));
        let unknown = initialize_result(&json!({ "protocolVersion": "1999-01-01" }));
        let silent = initialize_result(&json!({}));

        // Assert
        assert_eq!(asked["protocolVersion"], "2024-11-05");
        assert_eq!(newer["protocolVersion"], "2025-06-18");
        assert_eq!(
            unknown["protocolVersion"],
            *PROTOCOL_VERSIONS.last().expect("one")
        );
        assert_eq!(
            silent["protocolVersion"],
            *PROTOCOL_VERSIONS.last().expect("one")
        );
        assert_eq!(asked["serverInfo"]["name"], "pgokf-mcp");
        assert_eq!(asked["capabilities"]["tools"], json!({}));
    }

    #[test]
    fn a_name_from_the_caller_is_bounded_and_escaped_before_it_is_echoed() {
        // Arrange / Act / Assert: it reaches a log line, so a newline in it
        // must not become one.
        assert_eq!(echo("concept_search"), "\"concept_search\"");
        assert_eq!(echo("a\nb"), "\"a\\nb\"");
        let long = echo(&"x".repeat(ECHO_MAX * 4));
        assert!(long.chars().count() <= ECHO_MAX + 3, "{}", long.len());
        assert!(long.ends_with("…\""));
    }

    #[test]
    fn a_tool_result_is_one_text_block_carrying_the_json() {
        // Arrange / Act
        let ok = tool_result(&json!([{ "concept_id": "a" }]), false);
        let bad = tool_result(&json!("it went wrong"), true);

        // Assert
        assert_eq!(ok["isError"], false);
        assert_eq!(ok["content"][0]["type"], "text");
        assert!(
            ok["content"][0]["text"]
                .as_str()
                .expect("text")
                .contains("concept_id")
        );
        assert_eq!(bad["isError"], true);
    }
}
