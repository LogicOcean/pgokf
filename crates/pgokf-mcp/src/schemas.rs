// SPDX-License-Identifier: AGPL-3.0-only
//! JSON Schema 2020-12 contracts for catalog tools.
use serde_json::{Value, json};
pub(crate) const HOST_ONLY: &str = "x-okf-host-only";
fn catalog_tool_definitions() -> Value {
    json!([
        {
            "name": "concept_search",
            "description": "Rank catalog concepts by a full-text query. Optional structured filters narrow by concept type, tags (all-of), status, and trust tier.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "query": {"type": "string", "description": "The search query (websearch syntax)."},
                    "bundle_id": {"type": "integer", "description": "Restrict to one bundle id."},
                    "limit": {"type": "integer", "description": "Maximum hits (1..=500, default 20)."},
                    "type": {"type": "string", "description": "Exact concept type filter."},
                    "tags": {"type": "array", "items": {"type": "string"}, "description": "All-of tag containment filter."},
                    "status": {"type": "string", "description": "Provenance status filter."},
                    "trust_tier": {"type": "string", "description": "Provenance trust-tier filter."}
                },
                "required": ["query"]
            }
        },
        {
            "name": "find_similar",
            "description": "Content more-like-this: rank concepts by similarity to a seed concept's salient terms, excluding the seed.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "concept_id": {"type": "string", "description": "The seed concept id."},
                    "bundle_id": {"type": "integer", "description": "Bundle id (required if the concept id is ambiguous across bundles)."},
                    "limit": {"type": "integer", "description": "Maximum hits (1..=500, default 10)."}
                },
                "required": ["concept_id"]
            }
        },
        {
            "name": "concept_neighbors",
            "description": "Traverse resolved internal links out from a concept, returning reachable concepts with their shortest hop count and path.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "concept_id": {"type": "string", "description": "The start concept id."},
                    "max_hops": {"type": "integer", "description": "Traversal depth (>= 1, default 2; capped by pgokf.max_graph_hops)."},
                    "bundle_id": {"type": "integer", "description": "Bundle id (required if the concept id is ambiguous across bundles)."}
                },
                "required": ["concept_id"]
            }
        },
        {
            "name": "get_concept",
            "description": "Fetch a concept's stored core fields (path, type, title, description, tags, resource, body text) by id.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "concept_id": {"type": "string", "description": "The concept id."},
                    "bundle_id": {"type": "integer", "description": "Restrict to one bundle id."}
                },
                "required": ["concept_id"]
            }
        },
        {
            "name": "get_skill",
            "description": "Fetch an Agent Skills package stored in the catalog: its name, description, package directory and hash, the complete SKILL.md frontmatter, the exact SKILL.md text, and the list of scripts, references, and assets it owns (build_workspace_plugin materializes the whole package byte for byte).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "bundle_id": {"type": "integer", "description": "The bundle id."},
                    "concept_id": {"type": "string", "description": "The skill's concept id (its SKILL.md path without .md, e.g. skills/deploy/SKILL)."}
                },
                "required": ["bundle_id", "concept_id"]
            }
        }
    ])
}

/// The plugin-building tools.
fn plugin_tool_definitions() -> Value {
    json!([
        {
            "name": "list_plugin_targets",
            "description": "List the targets a workspace plugin can be built for: agent-plugin (a portable Agent Plugins 1.0.0 directory with plugin.json, skills/, and mcp.json) and the per-harness layouts (claude-code, codex, copilot, hermes-agent, kimi, gemini-cli, cursor, agents, agents-md, ollama, generic), each with its kind (agent-plugin, skills, instruction-file, prompt-bundle, generic) and the documented directory it reads. An agent not listed is built with target 'custom' plus a harness description.",
            "inputSchema": {"type": "object", "properties": {}}
        },
        {
            "name": "build_workspace_plugin",
            "description": "Build an agent plugin from a catalog selection: an Agent Skills package (SKILL.md plus one reference file per concept), an AGENTS.md instruction file, an Ollama prompt bundle, or a generic index-plus-files tree, always with okf-workspace.yaml (the selection) and okf-workspace.lock (catalog snapshot and content hashes). Pass output_dir to write the tree into a workspace; otherwise the files come back inline.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "target": {"type": "string", "description": "A target id from list_plugin_targets, or 'custom' with a harness description for an agent the registry does not know."},
                    "harness": {"type": "object", "description": "With target 'custom': the agent's display name, the kind of tree (agent-plugin, skills, instruction-file, prompt-bundle, generic; default skills), and for a skills package the directory the agent reads skills from, relative to the workspace root (for example .acme/skills).", "properties": {"label": {"type": "string"}, "kind": {"type": "string", "enum": ["agent-plugin", "skills", "instruction-file", "prompt-bundle", "generic"]}, "skills_dir": {"type": "string"}}, "required": ["label"]},
                    "all": {"type": "boolean", "description": "Start from every visible concept (bounded by the limit) instead of nothing, so a selection narrowed only by types, tags, or a query - or by nothing at all - is expressible without naming a bundle."},
                    "name": {"type": "string", "description": "Package name (lowercase letters, digits, hyphens; default okf-knowledge)."},
                    "title": {"type": "string", "description": "Display title for the index (defaults to the name)."},
                    "bundle_ids": {"type": "array", "items": {"type": "integer"}, "description": "Restrict to these bundle ids."},
                    "concept_ids": {"type": "array", "items": {"type": "string"}, "description": "Include exactly these concept ids (within the selected bundles)."},
                    "picks": {"type": "array", "items": {"type": "string"}, "description": "Specific files by identity, as 'bundle_id:concept_id' strings (a skill's SKILL.md id copies the whole package; a script or reference id copies that file). Picks are added to whatever the other selectors match and are never cut by the limit."},
                    "stale_policy": {"type": "string", "enum": ["warn", "exclude"], "description": "What to do with concepts the catalog reports as not fresh (default warn): warn keeps them, labelled - a banner on each reconstructed document, a generated <name>.stale-warning.md beside every exact-bytes file, which is never modified, and a top-level FRESHNESS.md - with states and reasons in the manifests; exclude drops them and refuses the build, enumerating the stale ids and reasons, when an exact pick, a seed, or a required closure node is stale, or nothing fresh remains."},
                    "seeds": {"type": "array", "items": {"type": "string"}, "description": "Closure seeds, as 'bundle_id:concept_id' strings: the build includes each seed and expands it through the catalog's typed relationships (requires the catalog's typed_relationships capability)."},
                    "relation_types": {"type": "array", "items": {"type": "string"}, "description": "With seeds: follow only these namespaced relationship types (for example 'docs:references'); empty follows every type. The names are the publisher's; the builder holds no vocabulary of its own."},
                    "direction": {"type": "string", "enum": ["outbound", "inbound", "both"], "description": "With seeds: which way the closure walks relationships (default outbound)."},
                    "hops": {"type": "integer", "description": "With seeds: how many relationship hops the closure walks (1..=8, default 2)."},
                    "require_closure": {"type": "boolean", "description": "With seeds: refuse the build when the closure cannot be completed - a traversed relationship whose target is unresolved, or a closure node the stale policy would drop."},
                    "tags": {"type": "array", "items": {"type": "string"}, "description": "All-of tag containment filter."},
                    "types": {"type": "array", "items": {"type": "string"}, "description": "Any-of concept type filter."},
                    "query": {"type": "string", "description": "Full-text query (websearch syntax); results are ranked."},
                    "verified_only": {"type": "boolean", "description": "Only human-reviewed or machine-confirmed concepts."},
                    "limit": {"type": "integer", "description": "Maximum concepts (1..=500, default 100)."},
                    "base_model": {"type": "string", "description": "Ollama only: the Modelfile FROM line (default llama3.1)."},
                    "components": {"type": "array", "items": {"type": "string", "enum": ["mcp", "guide", "tools"]}, "description": "Extra parts: mcp (the harness's MCP server config for pgokf-mcp; the connection string is never written), guide (how to use the catalog: identities, trust tiers, MCP tools, JSON API), tools (okf.sh helper over the JSON API). Default: all three."},
                    "mcp_command": {"type": "string", "description": "How the harness starts the MCP server (default pgokf-mcp). Alternative to mcp_url."},
                    "mcp_url": {"type": "string", "description": "Point the harness at a pgokf-mcp --http endpoint (for example https://catalog.example/mcp) instead of a local stdio server. The bearer token is referenced, never written: the entry expands OKF_MCP_TOKEN where the harness documents an expansion form, names that variable where it documents one, and otherwise becomes a fragment to merge with a placeholder in it."},
                    "web_url": {"type": "string", "description": "Base URL of the pgokf web UI, for the guide and the helper script."},
                    "output_dir": {"type": "string", HOST_ONLY: true, "description": "Write the tree under this workspace directory instead of returning contents. Local transports only: over HTTP the tree would be written on the server, so it is refused."},
                    "overwrite": {"type": "boolean", HOST_ONLY: true, "description": "With output_dir: replace files that already exist, including an existing AGENTS.md (default false; symbolic links are never followed)."}
                },
                "required": ["target"]
            }
        },
        {
            "name": "check_workspace_plugin_freshness",
            "description": "Compare a built plugin's okf-workspace.lock with the live catalog: the pinned bundle generations, sync hashes, and freshness states against what the catalog reports now. Answers current, stale, retired, or unknown, with reasons per bundle. A downloaded plugin cannot update itself; rebuild it with build_workspace_plugin when this says stale.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "lock": {"type": "string", "description": "The full content of the plugin's okf-workspace.lock file."}
                },
                "required": ["lock"]
            }
        }
    ])
}

fn write_tool_definitions() -> Value {
    json!([
        {
            "name": "list_bundles",
            "description": "List the bundles in the catalog with their id, name, kind (content: written through this API; filesystem: synced from a directory the database server reads), state, and file count. Use it to find the bundle name that put_document writes into.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "include_retired": {"type": "boolean", "description": "Include retired bundles (default false)."}
                }
            }
        },
        {
            "name": "put_document",
            "description": "Write one Markdown document into a content bundle, creating it or replacing the one at that path. The text is a complete OKF document: YAML frontmatter (type and title are required) then the body. Any 'verified' the document claims is set aside rather than believed - a verification is granted by a human approver reviewing it - and 'generated' is stamped with this token, so what you write is machine-generated until a person reviews it. The whole bundle is rewritten from the catalog's stored sources, so the catalog must keep them (store_source).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "bundle": {"type": "string", "description": "The content bundle's name (see list_bundles). Alternative to bundle_id."},
                    "bundle_id": {"type": "integer", "description": "The content bundle's id. Alternative to bundle."},
                    "path": {"type": "string", "description": "Path within the bundle, ending in .md (for example runbooks/failover.md). It becomes the concept id without the extension."},
                    "text": {"type": "string", "description": "The complete document: '---', YAML frontmatter, '---', then the Markdown body."}
                },
                "required": ["path", "text"]
            }
        },
        {
            "name": "delete_document",
            "description": "Remove one document from a content bundle. The rest of the bundle is rewritten unchanged; the document's history is not recoverable through this API.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "bundle": {"type": "string", "description": "The content bundle's name. Alternative to bundle_id."},
                    "bundle_id": {"type": "integer", "description": "The content bundle's id. Alternative to bundle."},
                    "path": {"type": "string", "description": "The path to remove, as list_bundles' concepts and get_concept report it."}
                },
                "required": ["path"]
            }
        },
        {
            "name": "create_content_bundle",
            "description": "Create an empty content bundle to write documents into. A content bundle lives entirely in the catalog: nothing on any filesystem backs it, and put_document is how its documents get there.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "name": {"type": "string", "description": "The bundle's name, unique among bundles."}
                },
                "required": ["name"]
            }
        },
        {
            "name": "refresh_bundle",
            "description": "Re-read a filesystem bundle from its directory and index what changed. Content bundles have no source to re-read and are refused; write to them with put_document instead.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "bundle": {"type": "string", "description": "The bundle's name. Alternative to bundle_id."},
                    "bundle_id": {"type": "integer", "description": "The bundle's id. Alternative to bundle."}
                }
            }
        },
        {
            "name": "set_bundle_state",
            "description": "Enable, disable, retire, or bring back a bundle. Disabled and retired bundles leave search and browsing; both are reversible, and neither deletes anything.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "bundle": {"type": "string", "description": "The bundle's name. Alternative to bundle_id."},
                    "bundle_id": {"type": "integer", "description": "The bundle's id. Alternative to bundle."},
                    "state": {"type": "string", "enum": ["enabled", "disabled", "retired", "active"], "description": "enabled/disabled toggle whether it is searched; retired takes it out of the catalog reversibly, active brings a retired one back."}
                },
                "required": ["state"]
            }
        }
    ])
}

pub(crate) fn definitions() -> std::collections::BTreeMap<String, Value> {
    let mut definitions = std::collections::BTreeMap::new();
    for group in [
        catalog_tool_definitions(),
        plugin_tool_definitions(),
        write_tool_definitions(),
    ] {
        for mut tool in group.as_array().expect("tool array").iter().cloned() {
            let name = tool["name"].as_str().expect("tool name").to_owned();
            let schema = &mut tool["inputSchema"];
            schema["$schema"] = json!("https://json-schema.org/draft/2020-12/schema");
            strict_objects(schema);
            let props = schema["properties"].as_object_mut().expect("properties");
            for (key, property) in props.iter_mut() {
                if property["type"] == "array" {
                    property["maxItems"] = json!(10_000);
                }
                if property["type"] == "string" {
                    property["maxLength"] = json!(1_048_576);
                }
                match key.as_str() {
                    "bundle_id" => {
                        let description = property.get("description").cloned();
                        *property = bundle_id();
                        if let Some(description) = description {
                            property["description"] = description;
                        }
                    }
                    "concept_id" => {
                        property["minLength"] = json!(1);
                        property["maxLength"] = json!(512);
                    }
                    "max_hops" => {
                        property["minimum"] = json!(1);
                        property["maximum"] = json!(i32::MAX);
                    }
                    "hops" => {
                        property["minimum"] = json!(1);
                        property["maximum"] = json!(8);
                    }
                    "limit" => {
                        property["minimum"] = json!(1);
                        property["maximum"] = json!(500);
                    }
                    "bundle_ids" => {
                        property["items"] = bundle_id();
                    }
                    "name" | "bundle" | "path" => {
                        property["minLength"] = json!(1);
                    }
                    _ => (),
                }
            }
            if name == "concept_search" {
                props.insert(
                    "cursor".to_owned(),
                    json!({"type":"string","maxLength":1024}),
                );
            }
            if [
                "put_document",
                "delete_document",
                "refresh_bundle",
                "set_bundle_state",
            ]
            .contains(&name.as_str())
            {
                schema["oneOf"] = json!([{"required":["bundle"],"not":{"required":["bundle_id"]}}, {"required":["bundle_id"],"not":{"required":["bundle"]}}]);
            }
            if name == "build_workspace_plugin" {
                schema["properties"]["target"]["enum"] = json!([
                    "agent-plugin",
                    "claude-code",
                    "codex",
                    "copilot",
                    "hermes-agent",
                    "kimi",
                    "gemini-cli",
                    "cursor",
                    "agents",
                    "agents-md",
                    "ollama",
                    "generic",
                    "custom"
                ]);
                schema["allOf"] = json!([{"if":{"properties":{"target":{"const":"custom"}},"required":["target"]},"then":{"required":["harness"]}}]);
            }
            definitions.insert(name, tool);
        }
    }
    definitions.insert("traverse_concepts".to_owned(), traversal_definition());
    definitions
}

fn traversal_definition() -> Value {
    let mut tool = json!({
        "name":"traverse_concepts", "description":"Traverse typed relationships or resolved Markdown links with bounded depth and result size.",
        "inputSchema": {
            "$schema":"https://json-schema.org/draft/2020-12/schema", "type":"object", "additionalProperties":false,
            "properties": {"bundle_id":bundle_id(), "concept_id":{"type":"string","minLength":1,"maxLength":512},
                "graph":{"enum":["relationships","links"]}, "direction":{"enum":["outbound","inbound","both"]},
                "relation_types":{"type":"array","maxItems":10_000,"items":{"type":"string"}},
                "max_hops":{"type":"integer","minimum":1,"maximum":2_147_483_647},
                "limit":{"type":"integer","minimum":1,"maximum":10_000}},
            "required":["bundle_id","concept_id"]}});
    tool["inputSchema"]["allOf"] = json!([{
        "if":{"properties":{"graph":{"const":"links"}},"required":["graph"]},
        "then":{"properties":{"direction":{"const":"outbound"},"relation_types":{"maxItems":0}}}
    }]);
    tool
}

fn strict_objects(schema: &mut Value) {
    if schema["type"] == "object" {
        schema["additionalProperties"] = json!(false);
    }
    if let Some(props) = schema.get_mut("properties").and_then(Value::as_object_mut) {
        for child in props.values_mut() {
            strict_objects(child);
        }
    }
    if let Some(items) = schema.get_mut("items") {
        strict_objects(items);
    }
}

fn bundle_id() -> Value {
    json!({"type":"integer","minimum":1,"maximum":9_223_372_036_854_775_807_i64})
}
fn string() -> Value {
    json!({"type":"string"})
}
fn nullable_string() -> Value {
    json!({"type":["string","null"]})
}
fn integer() -> Value {
    json!({"type":"integer"})
}
fn boolean() -> Value {
    json!({"type":"boolean"})
}
fn array(items: Value) -> Value {
    let mut schema = json!({"type":"array"});
    schema["items"] = items;
    schema
}
fn object(properties: Value, required: &[&str]) -> Value {
    let mut schema = json!({"type":"object","required":required,"additionalProperties":false});
    schema["properties"] = properties;
    schema
}
fn linked(mut schema: Value) -> Value {
    schema["properties"]["uri"] = json!({"type":"string","maxLength":2048});
    schema
}
fn freshness() -> Value {
    object(
        json!({"state":string(),"reasons":array(string())}),
        &["state", "reasons"],
    )
}
fn sync() -> Value {
    object(
        json!({"bundle_id":bundle_id(),"path":string(),"added":integer(),"removed":integer(),
        "total":integer(),"unchanged":integer(),"updated":integer()}),
        &[
            "bundle_id",
            "path",
            "added",
            "removed",
            "total",
            "unchanged",
            "updated",
        ],
    )
}

pub(crate) fn output(handler: crate::registry::Handler) -> Value {
    use crate::registry::Handler;
    let success = match handler {
        Handler::ConceptSearch | Handler::FindSimilar => array(linked(object(
            json!({
            "bundle_id":bundle_id(),"concept_id":string(),"path":string(),"title":nullable_string(),
            "type":nullable_string(),"headline":nullable_string(),"rank":{"type":"number"}}),
            &[
                "bundle_id",
                "concept_id",
                "path",
                "title",
                "type",
                "headline",
                "rank",
            ],
        ))),
        Handler::ConceptNeighbors => array(linked(object(
            json!({"source_id":string(),"neighbor_id":string(),
            "hops":integer(),"path":array(string()),"title":nullable_string()}),
            &["source_id", "neighbor_id", "hops", "path", "title"],
        ))),
        Handler::TraverseConcepts => array(linked(object(
            json!({"bundle_id":bundle_id(),"concept_id":string(),
            "catalog_generation":integer(),"direction":{"enum":["outbound","inbound","both"]},
            "hops":integer(),"path":array(object(json!({"bundle_id":bundle_id(),"concept_id":string()}), &["bundle_id","concept_id"])),"relation_type":nullable_string(),"title":nullable_string(),
            "freshness":freshness()}),
            &[
                "bundle_id",
                "concept_id",
                "catalog_generation",
                "direction",
                "hops",
                "path",
                "relation_type",
                "title",
                "freshness",
                "uri",
            ],
        ))),
        Handler::GetConcept => concept_output(),
        Handler::GetSkill => skill_output(),
        Handler::ListBundles => array(object(
            json!({"id":bundle_id(),"name":string(),"source_type":string(),
            "enabled":boolean(),"retired":boolean(),"file_count":integer()}),
            &[
                "id",
                "name",
                "source_type",
                "enabled",
                "retired",
                "file_count",
            ],
        )),
        Handler::CreateContentBundle => object(
            json!({"created":string(),"sync":sync()}),
            &["created", "sync"],
        ),
        Handler::RefreshBundle => object(
            json!({"bundle_id":bundle_id(),"bundle":string(),"sync":sync()}),
            &["bundle_id", "bundle", "sync"],
        ),
        Handler::SetBundleState => object(
            json!({"bundle_id":bundle_id(),"bundle":string(),"state":{"enum":["enabled","disabled","retired","active"]}}),
            &["bundle_id", "bundle", "state"],
        ),
        Handler::DeleteDocument => object(
            json!({"bundle_id":bundle_id(),"bundle":string(),"path":string(),"deleted":boolean(),"sync":sync()}),
            &["bundle_id", "bundle", "path", "deleted", "sync"],
        ),
        Handler::PutDocument => object(
            json!({"bundle_id":bundle_id(),"bundle":string(),"path":string(),"replaced":boolean(),
            "written_by":string(),"generated_by":string(),"verifications_set_aside":integer(),"sync":sync(),"note":string()}),
            &[
                "bundle_id",
                "bundle",
                "path",
                "replaced",
                "written_by",
                "generated_by",
                "verifications_set_aside",
                "sync",
                "note",
            ],
        ),
        Handler::CheckWorkspacePluginFreshness => object(
            json!({"name":nullable_string(),"status":{"enum":["current","stale","retired","unknown"]},
            "reasons":array(string()),"bundles":array(object(json!({"bundle_id":bundle_id(),"name":nullable_string(),
                "status":{"enum":["current","stale","retired","unknown"]},"reasons":array(string())}), &["bundle_id","name","status","reasons"]))}),
            &["name", "status", "reasons", "bundles"],
        ),
        Handler::ListPluginTargets => plugin_targets(),
        Handler::BuildWorkspacePlugin => json!({"oneOf":[plugin_build(), build_refusal()]}),
    };
    json!({"$schema":"https://json-schema.org/draft/2020-12/schema","oneOf":[success,
        object(json!({"code":{"enum":["invalid_argument","not_found","permission_denied","writer_unavailable","conflict","build_refused","invalid_cursor","stale_cursor","invalid_uri","resource_unavailable","skill_not_listable","internal"]},"message":string()}), &["code","message"])]})
}

fn plugin_targets() -> Value {
    let remote = object(
        json!({"path":string(),"auto_loaded":boolean(),"merge_into":nullable_string(),"url_key":string(),
        "type":nullable_string(),"token":string(),"token_env":nullable_string(),"documented_at":string()}),
        &[
            "path",
            "auto_loaded",
            "merge_into",
            "url_key",
            "type",
            "token",
            "token_env",
            "documented_at",
        ],
    );
    let mcp = object(
        json!({"path":string(),"auto_loaded":boolean(),"remote":{"anyOf":[remote,{"type":"null"}]}}),
        &["path", "auto_loaded", "remote"],
    );
    array(object(
        json!({"id":string(),"label":string(),"kind":string(),"kind_label":string(),"shape":string(),
        "root":string(),"documented_at":string(),"verified":string(),"notes":string(),"mcp":{"anyOf":[mcp,{"type":"null"}]}}),
        &[
            "id",
            "label",
            "kind",
            "kind_label",
            "shape",
            "root",
            "documented_at",
            "verified",
            "notes",
            "mcp",
        ],
    ))
}

fn plugin_build() -> Value {
    let concept = object(
        json!({"bundle_id":bundle_id(),"concept_id":string(),"title":string(),"type":string(),
        "trust_tier":string(),"exact_source":boolean(),"freshness":string()}),
        &[
            "bundle_id",
            "concept_id",
            "title",
            "type",
            "trust_tier",
            "exact_source",
            "freshness",
        ],
    );
    let file = object(
        json!({"path":string(),"bytes":integer(),"sha256":string(),"executable":boolean()}),
        &["path", "bytes", "sha256", "executable"],
    );
    let bytes = object(
        json!({"encoding":{"const":"base64"},"data":string()}),
        &["encoding", "data"],
    );
    object(
        json!({"target":string(),"name":string(),"root":string(),"concept_count":integer(),"size_bytes":integer(),
        "concepts":array(concept),"files":array(file),"contents":{"type":"object","additionalProperties":{"anyOf":[string(),bytes]}},
        "written":array(string()),"note":string(),"warnings":array(excluded()),"excluded":array(excluded())}),
        &[
            "target",
            "name",
            "root",
            "concept_count",
            "size_bytes",
            "concepts",
            "files",
        ],
    )
}

fn excluded() -> Value {
    object(
        json!({"bundle_id":bundle_id(),"concept_id":string(),"reasons":array(string()),"state":string(),"role":string()}),
        &["bundle_id", "concept_id", "state", "role", "reasons"],
    )
}

fn build_refusal() -> Value {
    let identity = object(
        json!({"bundle_id":bundle_id(),"concept_id":string()}),
        &["bundle_id", "concept_id"],
    );
    let edge = object(
        json!({"source_bundle_id":bundle_id(),"source_concept_id":string(),"relation_type":string(),
        "direction":string(),"target_bundle_id":{"type":["integer","null"]},"target_concept_id":nullable_string(),
        "external_target":nullable_string()}),
        &[
            "source_bundle_id",
            "source_concept_id",
            "relation_type",
            "direction",
            "target_bundle_id",
            "target_concept_id",
            "external_target",
        ],
    );
    let mut variants = Vec::new();
    for (reason, field, schema) in [
        ("stale_required", "concepts", array(excluded())),
        ("no_fresh_concepts", "concepts", array(excluded())),
        ("unresolved_required", "edges", array(edge)),
        ("seed_not_found", "seeds", array(identity.clone())),
        ("closure_nodes_missing", "nodes", array(identity)),
        ("closure_exceeds_limit", "limit", integer()),
    ] {
        let mut properties =
            json!({"reason":{"const":reason},"refused":{"const":true},"detail":string()});
        properties[field] = schema;
        variants.push(object(properties, &["reason", "refused", "detail", field]));
    }
    json!({"oneOf":variants})
}

fn skill_output() -> Value {
    linked(object(
        json!({"bundle_id":bundle_id(),"concept_id":string(),"name":string(),
            "description":string(),"package_root":string(),"file_hash":string(),"package_hash":string(),
            "visibility":string(),"skill_md":string(),"agent_skill":{"type":"object"},
            "resources":array(object(json!({"byte_size":integer(),"class":{"enum":["script","reference","asset"]},
                "concept_id":string(),"file_hash":string(),"media_type":nullable_string(),"language":nullable_string(),
                "path":string(),"sha256":string()}), &["byte_size","class","concept_id","file_hash","path","sha256"]))}),
        &[
            "bundle_id",
            "concept_id",
            "name",
            "description",
            "package_root",
            "file_hash",
            "package_hash",
            "visibility",
            "skill_md",
            "agent_skill",
            "resources",
        ],
    ))
}

fn concept_output() -> Value {
    array(linked(object(
        json!({"bundle_id":bundle_id(),"concept_id":string(),
            "path":string(),"type":string(),"title":nullable_string(),"description":nullable_string(),
            "tags":array(string()),"resource":nullable_string(),"body_text":nullable_string(),"modified_at":nullable_string()}),
        &[
            "bundle_id",
            "concept_id",
            "path",
            "type",
            "title",
            "description",
            "tags",
            "resource",
            "body_text",
            "modified_at",
        ],
    )))
}
