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

use serde_json::{Value, json};

use crate::catalog::Catalog;
use crate::registry::{self, Availability};
use crate::rpc::{INVALID_PARAMS, METHOD_NOT_FOUND, Request, Response};
use crate::service::{AuthMode, CatalogCommandService, CatalogReadService, PrincipalContext};
use crate::tokens::Bearer;
#[cfg(test)]
use crate::tokens::{Role, ToolAccess as _};

/// The MCP protocol revisions this server implements, oldest first. The
/// messages are the same across all three for the tools surface; the last
/// is what a client that asks for something else is answered with.
pub(crate) const PROTOCOL_VERSIONS: &[&str] = &["2024-11-05", "2025-03-26", "2025-06-18"];

/// JSON-RPC error code for a call the caller's role does not allow. The
/// specification reserves -32000 to -32099 for the server's own errors.
#[cfg(test)]
pub(crate) const NOT_PERMITTED: i64 = crate::policy::NOT_PERMITTED;

/// How much of a caller-supplied name is quoted back into a log line or an
/// error message. The rest is the caller's to choose, so it is bounded and
/// escaped rather than echoed.
#[cfg(test)]
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
    fn principal(&self) -> PrincipalContext {
        match self {
            Self::Local => PrincipalContext::local(),
            Self::Remote(bearer) => PrincipalContext {
                principal_id: bearer.principal_id.clone(),
                token_name: Some(bearer.name.clone()),
                tenant: bearer.tenant.clone(),
                role: bearer.role,
                auth_mode: AuthMode::BearerToken,
            },
        }
    }

    fn tools_with_policy(&self, policy: &crate::policy::PolicyService) -> Value {
        let principal = self.principal();
        Value::Array(
            policy
                .visible_tools(&principal)
                .into_iter()
                .map(|spec| spec.definition(principal.is_remote()))
                .collect(),
        )
    }

    #[cfg(test)]
    fn tools(&self) -> Value {
        self.tools_with_policy(&crate::policy::PolicyService::new(true, true))
    }

    fn refuse(&self, tool: &str, arguments: &Value) -> Option<(i64, String)> {
        crate::policy::PolicyService::new(true, true).legacy_refusal(
            &self.principal(),
            tool,
            arguments,
        )
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
    let modern = crate::modern::is_modern(request);
    if modern && let Err(response) = crate::modern::validate(request) {
        return response;
    }
    if !modern && request.method.starts_with("skills/") {
        return Response::error(
            id,
            METHOD_NOT_FOUND,
            "Skills extension requires MCP 2026-07-28",
        );
    }
    let mut response = match request.method.as_str() {
        "server/discover" => Response::success(id, crate::modern::discover(true, true)),
        "initialize" => Response::success(id, initialize_result(&request.params)),
        "tools/list" => Response::success(
            id,
            json!({ "tools": tool_definitions(catalog, caller, modern) }),
        ),
        "tools/call" => tools_call(catalog, caller, &request.params, id, modern).await,
        "resources/list"
        | "resources/templates/list"
        | "resources/read"
        | "skills/list"
        | "skills/get" => {
            let principal = caller.principal();
            match CatalogReadService::new(catalog, &principal)
                .method(&request.method, &request.params)
                .await
            {
                Ok(value) => Response::success(id, value),
                Err(error) => Response::error_data(
                    id,
                    INVALID_PARAMS,
                    error.message(),
                    json!({"code":error.code()}),
                ),
            }
        }
        "ping" => Response::success(id, json!({})),
        other => Response::error(
            id,
            METHOD_NOT_FOUND,
            format!("unknown method {}", echo(other)),
        ),
    };
    if modern {
        crate::modern::decorate(&request.method, &mut response);
    }
    response
}

fn tool_definitions(catalog: &Catalog, caller: &Caller, modern: bool) -> Value {
    let mut tools = caller.tools_with_policy(&catalog.policy());
    if !modern && let Some(tools) = tools.as_array_mut() {
        for tool in tools {
            let schema = tool["outputSchema"].take();
            tool["outputSchema"] = json!({"$schema":"https://json-schema.org/draft/2020-12/schema",
                "type":"object","properties":{"value":schema},"required":["value"],"additionalProperties":false});
        }
    }
    tools
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
    let capabilities = if version == "2025-06-18" {
        json!({"tools":{},"resources":{}})
    } else {
        json!({"tools":{}})
    };
    json!({
        "protocolVersion": version,
        "capabilities": capabilities,
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
async fn tools_call(
    catalog: &Catalog,
    caller: &Caller,
    params: &Value,
    id: Value,
    modern: bool,
) -> Response {
    let Some(name) = params.get("name").and_then(Value::as_str) else {
        return Response::error(id, INVALID_PARAMS, "tools/call requires a string 'name'");
    };
    let empty = json!({});
    let arguments = params.get("arguments").unwrap_or(&empty);
    if !arguments.is_object() {
        return Response::error(id, INVALID_PARAMS, "tool arguments must be an object");
    }

    if let Some((code, message)) = caller.refuse(name, arguments) {
        caller.note("was refused", name);
        return Response::error(id, if modern { INVALID_PARAMS } else { code }, message);
    }
    caller.note("called", name);

    let Some(spec) = registry::by_name(name) else {
        if modern {
            return Response::error(id, INVALID_PARAMS, "unknown tool");
        }
        let message = format!("unknown tool '{name}'");
        let mut result = tool_result(&json!(message), true);
        result["structuredContent"] =
            json!({"value":{"code":"invalid_argument", "message":message}});
        return Response::success(id, result);
    };
    let principal = caller.principal();
    let outcome = if spec.availability == Availability::Reader {
        CatalogReadService::new(catalog, &principal)
            .call(spec, arguments)
            .await
    } else {
        CatalogCommandService::new(catalog, &principal)
            .call(spec, arguments)
            .await
    };
    let mut result = tool_result(&outcome.value, outcome.is_error);
    if let Some(legacy) = outcome.legacy_value {
        result["content"][0]["text"] =
            json!(serde_json::to_string_pretty(&legacy).unwrap_or_default());
    }
    if modern && !outcome.is_error {
        add_resource_links(&mut result, name, arguments);
    }
    if let Some(cursor) = outcome.next_cursor {
        result["_meta"] = json!({"io.logicocean.pgokf/nextCursor":cursor});
    }
    if let Some(evidence) = outcome.evidence {
        if result.get("_meta").is_none() {
            result["_meta"] = json!({});
        }
        result["_meta"]["io.logicocean.pgokf/catalogEvidence"] = evidence;
    }
    if !modern {
        result["structuredContent"] = json!({"value":result["structuredContent"].take()});
    }
    Response::success(id, result)
}

fn add_resource_links(result: &mut Value, tool: &str, arguments: &Value) {
    let mut links = Vec::new();
    if matches!(
        tool,
        "concept_search"
            | "find_similar"
            | "get_concept"
            | "concept_neighbors"
            | "traverse_concepts"
    ) {
        if let Some(rows) = result["structuredContent"].as_array_mut() {
            for row in rows {
                let bundle = row
                    .get("bundle_id")
                    .and_then(Value::as_i64)
                    .or_else(|| arguments.get("bundle_id").and_then(Value::as_i64));
                let key = row
                    .get("concept_id")
                    .or_else(|| row.get("neighbor_id"))
                    .and_then(Value::as_str);
                if let (Some(bundle), Some(key)) = (bundle, key) {
                    let uri = crate::uri::ResourceUri::Concept {
                        bundle_id: bundle,
                        concept_id: key.into(),
                    }
                    .render();
                    if let Ok(uri) = uri {
                        links.push(json!({"type":"resource_link","uri":uri,"name":key,
                            "mimeType":"application/vnd.pgokf.concept+json"}));
                        row["uri"] = json!(uri);
                    }
                }
            }
        }
    } else if tool == "get_skill" {
        let data = &result["structuredContent"];
        if let (Some(bundle), Some(key), Some(name)) = (
            data["bundle_id"].as_i64(),
            data["concept_id"].as_str(),
            data["name"].as_str(),
        ) {
            let uri = crate::uri::ResourceUri::SkillFile {
                bundle_id: bundle,
                concept_id: key.into(),
                skill_name: name.into(),
                member_path: "SKILL.md".into(),
            }
            .render();
            if let Ok(uri) = uri {
                links.push(json!({"type":"resource_link","uri":uri,"name":name,"mimeType":"text/markdown"}));
                result["structuredContent"]["uri"] = json!(uri);
            }
        }
    }
    if let Some(contents) = result["content"].as_array_mut() {
        contents.extend(links);
    }
}

/// Wrap tool output as an MCP tool result: a single text content block
/// holding the JSON, with the `isError` flag.
pub(crate) fn tool_result(data: &Value, is_error: bool) -> Value {
    let text = serde_json::to_string_pretty(data).unwrap_or_else(|_| "null".to_owned());
    json!({
        "content": [ { "type": "text", "text": text } ],
        "isError": is_error,
        "structuredContent": data,
    })
}

#[cfg(test)]
fn host_only_arguments() -> &'static [String] {
    registry::host_only_arguments()
}

fn echo(text: &str) -> String {
    registry::echo(text)
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
            principal_id: "test-principal".into(),
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
        let writer = names(&remote(Role::Writer).tools());
        let admin = names(&remote(Role::Admin).tools());

        // Assert
        assert!(reader.contains(&"concept_search".to_owned()));
        assert!(!reader.contains(&"build_workspace_plugin".to_owned()));
        assert!(builder.contains(&"build_workspace_plugin".to_owned()));
        assert!(!builder.contains(&"put_document".to_owned()));
        assert!(writer.contains(&"put_document".to_owned()));
        assert!(
            !writer.contains(&"set_bundle_state".to_owned()),
            "managing bundles is the admin's"
        );
        assert!(admin.contains(&"set_bundle_state".to_owned()));
        assert!(reader.len() < builder.len());
        assert!(builder.len() < writer.len());
        assert!(writer.len() < admin.len());
        assert_eq!(local, admin, "stdio sees every tool there is");
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
