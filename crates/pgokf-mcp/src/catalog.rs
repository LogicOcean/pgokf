// SPDX-License-Identifier: AGPL-3.0-only
//! Catalog access and MCP tool definitions.
//!
//! Each MCP tool is backed by a single query against the shipped `pgokf` public
//! surface. Every query aggregates its result rows into one `jsonb` array with
//! `jsonb_agg(to_jsonb(...))`, so the server hands MCP a faithful JSON view of
//! exactly what the SQL functions return, with no per-column marshalling.

use std::path::Path;

use anyhow::{Context, Result, anyhow, bail};
use pgokf_workspace::{BuildOptions, Component, Profile, Selection, Target};
use serde_json::{Value, json};
use tokio_postgres::Client;
use tokio_postgres::types::ToSql;

/// Default `limit` for `concept_search` when the caller omits it.
const DEFAULT_SEARCH_LIMIT: i32 = 20;
/// Default `limit` for `find_similar` when the caller omits it.
const DEFAULT_SIMILAR_LIMIT: i32 = 10;
/// Default `max_hops` for `concept_neighbors` when the caller omits it.
const DEFAULT_MAX_HOPS: i32 = 2;
/// Largest plugin whose file contents are returned inline (bytes); above
/// it the caller is told to pass `output_dir` instead.
const INLINE_PLUGIN_BYTES: usize = 1_048_576;

/// A live catalog connection, optionally scoped to one tenant.
pub struct Catalog {
    client: Client,
    /// The database name, shown in plugin indexes as the catalog name.
    database_name: String,
    /// The session's tenant, carried into generated MCP configurations.
    tenant: Option<String>,
}

impl Catalog {
    /// Connect to PostgreSQL and, when `tenant` is set, apply it as the
    /// session's `pgokf.tenant` so tenant row-level security is enforced.
    ///
    /// `force_tls` (the `--tls` flag) requires an encrypted link; TLS is also
    /// negotiated for an `sslmode=require` connection URL. The connection driver
    /// task is spawned by the shared helper; its handle is detached because the
    /// server runs until stdin EOF and the process exit tears the task down.
    ///
    /// # Errors
    ///
    /// Returns an error if the connection or the tenant scoping fails.
    pub async fn connect(
        database_url: &str,
        tenant: Option<&str>,
        force_tls: bool,
    ) -> Result<Self> {
        let (client, _connection) = pgokf_pgconn::connect(database_url, force_tls)
            .await
            .context("connecting to PostgreSQL")?;

        if let Some(tenant) = tenant {
            pgokf_pgconn::set_tenant(&client, tenant).await?;
        }
        let database_name: String = client
            .query_one("SELECT current_database()", &[])
            .await
            .context("reading the database name")?
            .try_get(0)?;

        Ok(Self {
            client,
            database_name,
            tenant: tenant.map(str::to_owned),
        })
    }

    /// The MCP `tools/list` payload: the catalog tools this server exposes,
    /// each with a JSON-Schema description of its arguments.
    #[must_use]
    pub fn tool_definitions() -> Value {
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
            },
            {
                "name": "list_plugin_targets",
                "description": "List the agent harnesses a workspace plugin can be built for (claude-code, codex, hermes-agent, kimi, gemini-cli, cursor, agents, agents-md, ollama, generic) with the documented directory each one reads.",
                "inputSchema": {"type": "object", "properties": {}}
            },
            {
                "name": "build_workspace_plugin",
                "description": "Build an agent plugin from a catalog selection: an Agent Skills package (SKILL.md plus one reference file per concept), an AGENTS.md instruction file, an Ollama prompt bundle, or a generic index-plus-files tree, always with okf-workspace.yaml (the selection) and okf-workspace.lock (catalog snapshot and content hashes). Pass output_dir to write the tree into a workspace; otherwise the files come back inline.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "target": {"type": "string", "description": "A target id from list_plugin_targets."},
                        "name": {"type": "string", "description": "Package name (lowercase letters, digits, hyphens; default okf-knowledge)."},
                        "title": {"type": "string", "description": "Display title for the index (defaults to the name)."},
                        "bundle_ids": {"type": "array", "items": {"type": "integer"}, "description": "Restrict to these bundle ids."},
                        "concept_ids": {"type": "array", "items": {"type": "string"}, "description": "Include exactly these concept ids (within the selected bundles)."},
                        "tags": {"type": "array", "items": {"type": "string"}, "description": "All-of tag containment filter."},
                        "types": {"type": "array", "items": {"type": "string"}, "description": "Any-of concept type filter."},
                        "query": {"type": "string", "description": "Full-text query (websearch syntax); results are ranked."},
                        "verified_only": {"type": "boolean", "description": "Only human-reviewed or machine-confirmed concepts."},
                        "limit": {"type": "integer", "description": "Maximum concepts (1..=500, default 100)."},
                        "base_model": {"type": "string", "description": "Ollama only: the Modelfile FROM line (default llama3.1)."},
                        "components": {"type": "array", "items": {"type": "string", "enum": ["mcp", "guide", "tools"]}, "description": "Extra parts: mcp (the harness's MCP server config for pgokf-mcp; the connection string is never written), guide (how to use the catalog: identities, trust tiers, MCP tools, JSON API), tools (okf.sh helper over the JSON API). Default: all three."},
                        "mcp_command": {"type": "string", "description": "How the harness starts the MCP server (default pgokf-mcp)."},
                        "web_url": {"type": "string", "description": "Base URL of the pgokf web UI, for the guide and the helper script."},
                        "output_dir": {"type": "string", "description": "Write the tree under this workspace directory instead of returning contents."},
                        "overwrite": {"type": "boolean", "description": "With output_dir: replace files that already exist, including an existing AGENTS.md (default false; symbolic links are never followed)."}
                    },
                    "required": ["target"]
                }
            }
        ])
    }

    /// Dispatch one `tools/call` to the matching catalog query, returning the
    /// tool's JSON result data (a JSON array of rows).
    ///
    /// # Errors
    ///
    /// Returns an error for an unknown tool, an argument that is missing or the
    /// wrong type, or a database failure. The caller renders the error as an
    /// MCP `isError` tool result.
    pub async fn call_tool(&self, name: &str, arguments: &Value) -> Result<Value> {
        match name {
            "concept_search" => self.concept_search(arguments).await,
            "find_similar" => self.find_similar(arguments).await,
            "concept_neighbors" => self.concept_neighbors(arguments).await,
            "get_concept" => self.get_concept(arguments).await,
            "get_skill" => self.get_skill(arguments).await,
            "list_plugin_targets" => Ok(Self::list_plugin_targets()),
            "build_workspace_plugin" => self.build_workspace_plugin(arguments).await,
            other => bail!("unknown tool '{other}'"),
        }
    }

    async fn concept_search(&self, args: &Value) -> Result<Value> {
        let query = require_str(args, "query")?;
        let bundle_id = opt_i64(args, "bundle_id")?;
        let limit = opt_i32(args, "limit")?.unwrap_or(DEFAULT_SEARCH_LIMIT);
        let concept_type = opt_str(args, "type");
        let tags = opt_string_vec(args, "tags")?;
        let status = opt_str(args, "status");
        let trust_tier = opt_str(args, "trust_tier");

        self.fetch_json(
            "SELECT coalesce(jsonb_agg(to_jsonb(t) ORDER BY t.rank DESC, t.bundle_id, t.concept_id), '[]'::jsonb)
             FROM pgokf.concept_search($1, $2, $3, $4, $5, $6, $7) t",
            &[
                &query,
                &bundle_id,
                &limit,
                &concept_type,
                &tags,
                &status,
                &trust_tier,
            ],
        )
        .await
    }

    async fn find_similar(&self, args: &Value) -> Result<Value> {
        let concept_id = require_str(args, "concept_id")?;
        let bundle_id = opt_i64(args, "bundle_id")?;
        let limit = opt_i32(args, "limit")?.unwrap_or(DEFAULT_SIMILAR_LIMIT);

        self.fetch_json(
            "SELECT coalesce(jsonb_agg(to_jsonb(t) ORDER BY t.rank DESC, t.bundle_id, t.concept_id), '[]'::jsonb)
             FROM pgokf.find_similar($1, $2, $3) t",
            &[&concept_id, &bundle_id, &limit],
        )
        .await
    }

    async fn concept_neighbors(&self, args: &Value) -> Result<Value> {
        let concept_id = require_str(args, "concept_id")?;
        let max_hops = opt_i32(args, "max_hops")?.unwrap_or(DEFAULT_MAX_HOPS);
        let bundle_id = opt_i64(args, "bundle_id")?;

        self.fetch_json(
            "SELECT coalesce(jsonb_agg(to_jsonb(t) ORDER BY t.hops, t.neighbor_id), '[]'::jsonb)
             FROM pgokf.concept_neighbors($1, $2, $3) t",
            &[&concept_id, &max_hops, &bundle_id],
        )
        .await
    }

    async fn get_concept(&self, args: &Value) -> Result<Value> {
        let concept_id = require_str(args, "concept_id")?;
        let bundle_id = opt_i64(args, "bundle_id")?;

        self.fetch_json(
            "SELECT coalesce(jsonb_agg(to_jsonb(c) ORDER BY c.bundle_id), '[]'::jsonb)
             FROM (
                 SELECT bundle_id, id AS concept_id, path, type, title, description,
                        tags, resource, body_text, modified_at
                 FROM pgokf.concepts
                 WHERE id = $1 AND ($2::bigint IS NULL OR bundle_id = $2)
             ) c",
            &[&concept_id, &bundle_id],
        )
        .await
    }

    /// `pgokf.get_skill`: the package's metadata, resource listing, and the
    /// exact `SKILL.md` as text (it is UTF-8 by construction). An audited
    /// read, like every exact-byte retrieval.
    async fn get_skill(&self, args: &Value) -> Result<Value> {
        let bundle_id = opt_i64(args, "bundle_id")?
            .ok_or_else(|| anyhow!("missing required integer argument 'bundle_id'"))?;
        let concept_id = require_str(args, "concept_id")?;
        self.fetch_json(
            "SELECT (to_jsonb(s) - 'skill_md')
                    || jsonb_build_object('skill_md', convert_from(s.skill_md, 'UTF8'))
             FROM pgokf.get_skill($1, $2) AS s",
            &[&bundle_id, &concept_id],
        )
        .await
    }

    fn list_plugin_targets() -> Value {
        Value::Array(
            Profile::all()
                .iter()
                .map(|p| {
                    json!({
                        "id": p.id,
                        "label": p.label,
                        "shape": format!("{:?}", p.shape),
                        "root": p.root,
                        "documented_at": p.source,
                        "verified": p.verified,
                        "notes": p.notes,
                    })
                })
                .collect(),
        )
    }

    /// The selection the tool arguments describe.
    fn selection_from_args(args: &Value) -> Result<Selection> {
        Ok(Selection {
            bundle_ids: opt_i64_vec(args, "bundle_ids")?.unwrap_or_default(),
            concept_ids: opt_string_vec(args, "concept_ids")?.unwrap_or_default(),
            tags: opt_string_vec(args, "tags")?.unwrap_or_default(),
            types: opt_string_vec(args, "types")?.unwrap_or_default(),
            query: opt_str(args, "query").map(str::to_owned),
            verified_only: args
                .get("verified_only")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            limit: match opt_i64(args, "limit")? {
                None => None,
                Some(n)
                    if usize::try_from(n)
                        .is_ok_and(|n| (1..=pgokf_workspace::MAX_CONCEPTS).contains(&n)) =>
                {
                    usize::try_from(n).ok()
                }
                Some(_) => bail!(
                    "argument 'limit' must be between 1 and {}",
                    pgokf_workspace::MAX_CONCEPTS
                ),
            },
        })
    }

    /// The build options the tool arguments describe.
    fn options_from_args(&self, args: &Value, target: Target) -> Result<BuildOptions> {
        let components = match opt_string_vec(args, "components")? {
            None => Component::all().to_vec(),
            Some(ids) => ids
                .iter()
                .map(|id| {
                    Component::parse(id).ok_or_else(|| {
                        anyhow!("unknown component '{id}'; use mcp, guide, or tools")
                    })
                })
                .collect::<Result<Vec<_>>>()?,
        };
        Ok(BuildOptions {
            target,
            name: opt_str(args, "name").unwrap_or("okf-knowledge").to_owned(),
            title: opt_str(args, "title").map(str::to_owned),
            catalog_name: self.database_name.clone(),
            base_model: opt_str(args, "base_model").map(str::to_owned),
            components,
            mcp_command: opt_str(args, "mcp_command").map(str::to_owned),
            tenant: self.tenant.clone(),
            web_url: opt_str(args, "web_url").map(str::to_owned),
        })
    }

    async fn build_workspace_plugin(&self, args: &Value) -> Result<Value> {
        let target_id = require_str(args, "target")?;
        let target = Target::parse(target_id).ok_or_else(|| {
            anyhow!("unknown target '{target_id}'; list_plugin_targets names the supported ones")
        })?;
        let selection = Self::selection_from_args(args)?;
        let options = self.options_from_args(args, target)?;
        let plugin = pgokf_workspace::build(&self.client, &options, &selection).await?;

        let mut result = json!({
            "target": plugin.target,
            "name": plugin.name,
            "root": plugin.root,
            "concept_count": plugin.concept_count,
            "size_bytes": plugin.size(),
            "concepts": plugin.concepts.iter().map(|c| json!({
                "bundle_id": c.bundle_id,
                "concept_id": c.concept_id,
                "title": c.title,
                "type": c.concept_type,
                "trust_tier": c.trust_tier,
                "exact_source": c.exact,
            })).collect::<Vec<_>>(),
            "files": plugin.files.iter().map(|f| json!({
                "path": f.path,
                "bytes": f.bytes.len(),
                "sha256": f.sha256,
                "executable": f.executable,
            })).collect::<Vec<_>>(),
        });
        match opt_str(args, "output_dir") {
            Some(dir) => {
                let overwrite = args
                    .get("overwrite")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                let written = pgokf_workspace::write_to_dir(&plugin, Path::new(dir), overwrite)?;
                result["written"] = Value::Array(
                    written
                        .iter()
                        .map(|p| Value::String(p.display().to_string()))
                        .collect(),
                );
            }
            None if plugin.size() <= INLINE_PLUGIN_BYTES => {
                result["contents"] = Value::Object(
                    plugin
                        .files
                        .iter()
                        .map(|f| (f.path.clone(), inline_content(&f.bytes)))
                        .collect(),
                );
            }
            None => {
                result["note"] = Value::String(format!(
                    "the tree is {} bytes; pass output_dir to write it instead of returning it inline",
                    plugin.size()
                ));
            }
        }
        Ok(result)
    }

    /// Run a query whose single row / single column is a `jsonb` aggregate, and
    /// return it as a `serde_json::Value`.
    async fn fetch_json(&self, sql: &str, params: &[&(dyn ToSql + Sync)]) -> Result<Value> {
        let row = self
            .client
            .query_one(sql, params)
            .await
            .context("catalog query failed")?;
        Ok(row.get(0))
    }
}

/// A file's content for the inline result: UTF-8 text as a string, anything
/// else as `{"encoding": "base64", "data": ...}` so a binary asset (a PNG in
/// a skill package) comes back byte-exact instead of with replacement
/// characters.
fn inline_content(bytes: &[u8]) -> Value {
    match std::str::from_utf8(bytes) {
        Ok(text) => Value::String(text.to_owned()),
        Err(_) => json!({ "encoding": "base64", "data": base64_encode(bytes) }),
    }
}

/// Standard base64 (RFC 4648 §4, with padding).
fn base64_encode(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let word = chunk
            .iter()
            .enumerate()
            .fold(0u32, |acc, (i, b)| acc | (u32::from(*b) << (16 - 8 * i)));
        for i in 0..4 {
            if i <= chunk.len() {
                let index = ((word >> (18 - 6 * i)) & 0x3f) as usize;
                out.push(char::from(ALPHABET[index]));
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// Read a required string argument.
fn require_str<'a>(args: &'a Value, key: &str) -> Result<&'a str> {
    opt_str(args, key).ok_or_else(|| anyhow!("missing required string argument '{key}'"))
}

/// Read an optional string argument (absent or JSON null → `None`).
fn opt_str<'a>(args: &'a Value, key: &str) -> Option<&'a str> {
    args.get(key).and_then(Value::as_str)
}

/// Read an optional 64-bit integer argument.
fn opt_i64(args: &Value, key: &str) -> Result<Option<i64>> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(value) => value
            .as_i64()
            .map(Some)
            .ok_or_else(|| anyhow!("argument '{key}' must be an integer")),
    }
}

/// Read an optional 32-bit integer argument, range-checking the value.
fn opt_i32(args: &Value, key: &str) -> Result<Option<i32>> {
    match opt_i64(args, key)? {
        None => Ok(None),
        Some(value) => i32::try_from(value)
            .map(Some)
            .map_err(|_| anyhow!("argument '{key}' is out of range for a 32-bit integer")),
    }
}

/// Read an optional array-of-integers argument.
fn opt_i64_vec(args: &Value, key: &str) -> Result<Option<Vec<i64>>> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Array(items)) => items
            .iter()
            .map(|item| {
                item.as_i64()
                    .ok_or_else(|| anyhow!("argument '{key}' must be an array of integers"))
            })
            .collect::<Result<Vec<_>>>()
            .map(Some),
        Some(_) => Err(anyhow!("argument '{key}' must be an array of integers")),
    }
}

/// Read an optional array-of-strings argument.
fn opt_string_vec(args: &Value, key: &str) -> Result<Option<Vec<String>>> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Array(items)) => {
            let mut out = Vec::with_capacity(items.len());
            for item in items {
                let text = item
                    .as_str()
                    .ok_or_else(|| anyhow!("argument '{key}' must be an array of strings"))?;
                out.push(text.to_owned());
            }
            Ok(Some(out))
        }
        Some(_) => Err(anyhow!("argument '{key}' must be an array of strings")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inline_content_keeps_text_and_base64_encodes_binary() {
        // Arrange / Act
        let text = inline_content(b"echo ok\n");
        let binary = inline_content(b"\x89PNG\r\n\x1a\n");

        // Assert
        assert_eq!(text, Value::String("echo ok\n".to_owned()));
        assert_eq!(binary["encoding"], "base64");
        assert_eq!(binary["data"], "iVBORw0KGgo=");
    }

    #[test]
    fn base64_encode_matches_rfc_4648_vectors() {
        // Arrange / Act / Assert
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foo"), "Zm9v");
        assert_eq!(base64_encode(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn require_str_reads_a_present_string() {
        // Arrange
        let args = json!({"query": "widgets"});

        // Act
        let value = require_str(&args, "query").expect("present");

        // Assert
        assert_eq!(value, "widgets");
    }

    #[test]
    fn require_str_errors_when_absent() {
        // Arrange
        let args = json!({});

        // Act & Assert
        assert!(require_str(&args, "query").is_err());
    }

    #[test]
    fn opt_i32_rejects_an_out_of_range_value() {
        // Arrange: one past i32::MAX.
        let args = json!({"limit": i64::from(i32::MAX) + 1});

        // Act & Assert
        assert!(opt_i32(&args, "limit").is_err());
    }

    #[test]
    fn opt_string_vec_reads_a_string_array() {
        // Arrange
        let args = json!({"tags": ["a", "b"]});

        // Act
        let tags = opt_string_vec(&args, "tags")
            .expect("valid")
            .expect("present");

        // Assert
        assert_eq!(tags, vec!["a".to_owned(), "b".to_owned()]);
    }

    #[test]
    fn opt_string_vec_rejects_a_non_string_element() {
        // Arrange
        let args = json!({"tags": ["a", 3]});

        // Act & Assert
        assert!(opt_string_vec(&args, "tags").is_err());
    }

    #[test]
    fn opt_i64_vec_reads_integers_and_rejects_strings() {
        // Arrange
        let ok = json!({"bundle_ids": [1, 2]});
        let bad = json!({"bundle_ids": ["1"]});

        // Act & Assert
        assert_eq!(
            opt_i64_vec(&ok, "bundle_ids").expect("valid"),
            Some(vec![1, 2])
        );
        assert!(opt_i64_vec(&bad, "bundle_ids").is_err());
    }

    #[test]
    fn list_plugin_targets_names_every_documented_profile() {
        // Arrange & Act
        let targets = Catalog::list_plugin_targets();

        // Assert
        let ids: Vec<&str> = targets
            .as_array()
            .expect("array")
            .iter()
            .filter_map(|t| t["id"].as_str())
            .collect();
        assert!(
            ids.contains(&"claude-code") && ids.contains(&"agents-md") && ids.contains(&"ollama")
        );
        assert!(
            targets[0]["documented_at"]
                .as_str()
                .is_some_and(|s| s.starts_with("https://"))
        );
    }

    #[test]
    fn tool_definitions_lists_the_catalog_and_plugin_tools() {
        // Arrange & Act
        let tools = Catalog::tool_definitions();

        // Assert
        let names: Vec<&str> = tools
            .as_array()
            .expect("array")
            .iter()
            .filter_map(|tool| tool["name"].as_str())
            .collect();
        assert_eq!(
            names,
            vec![
                "concept_search",
                "find_similar",
                "concept_neighbors",
                "get_concept",
                "get_skill",
                "list_plugin_targets",
                "build_workspace_plugin",
            ],
        );
    }
}
