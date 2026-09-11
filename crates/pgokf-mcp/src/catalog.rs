// SPDX-License-Identifier: AGPL-3.0-only
//! Catalog access and MCP tool definitions.
//!
//! Each MCP tool is backed by a single query against the shipped `pgokf` public
//! surface. Every query aggregates its result rows into one `jsonb` array with
//! `jsonb_agg(to_jsonb(...))`, so the server hands MCP a faithful JSON view of
//! exactly what the SQL functions return, with no per-column marshalling.

use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use pgokf_workspace::{
    BuildOptions, Component, ConceptRef, CustomHarness, Direction, Profile, Selection, Shape,
    StalePolicy, TOKEN_ENV, Target, TokenRef,
};
use serde_json::{Value, json};
use tokio::sync::Mutex;
use tokio::task::JoinHandle;
use tokio_postgres::Client;
use tokio_postgres::types::ToSql;

use crate::write::{self, WriterConn};

/// Default `limit` for `concept_search` when the caller omits it.
const DEFAULT_SEARCH_LIMIT: i32 = 20;
/// Default `limit` for `find_similar` when the caller omits it.
const DEFAULT_SIMILAR_LIMIT: i32 = 10;
/// Default `max_hops` for `concept_neighbors` when the caller omits it.
const DEFAULT_MAX_HOPS: i32 = 2;
/// How long a write waits for another writer of the same bundle before it
/// gives up, in milliseconds.
const WRITER_LOCK_WAIT_MS: i32 = 15_000;
/// The bound on a writer's statements, in milliseconds, applied on every
/// transport; the HTTP transport lowers it to its own request budget.
const WRITER_STATEMENT_MS: i32 = 50_000;

/// Largest plugin whose file contents are returned inline (bytes); above
/// it the caller is told to narrow the selection.
const INLINE_PLUGIN_BYTES: usize = 1_048_576;

/// The schema keyword marking an argument that acts on the host this server
/// runs on rather than on the caller's. A transport that carries requests
/// from somewhere else refuses every argument marked with it, and derives
/// that set from the schemas below rather than keeping its own list, so a
/// new host-only argument is refused the day it is added.
pub const HOST_ONLY: &str = "x-okf-host-only";

/// A live catalog connection, optionally scoped to one tenant.
pub struct Catalog {
    client: Client,
    /// A second reader connection `build_workspace_plugin` holds for the
    /// whole build: the build runs inside one repeatable-read transaction,
    /// and the shared `client` pipelines requests from concurrent callers,
    /// so the transaction cannot live there. The mutex serializes builds.
    build: Mutex<Client>,
    /// The build connection's driver, folded into [`Catalog::take_driver`].
    build_driver: Option<JoinHandle<()>>,
    /// A `pgokf_writer` connection, when the operator gave one: what the
    /// `writer` and `admin` roles need, and what this server does without
    /// entirely when it is not set.
    writer: Option<WriterConn>,
    /// The writer connection's driver, waited on beside the reader's.
    writer_driver: Option<JoinHandle<()>>,
    /// The database name, shown in plugin indexes as the catalog name.
    database_name: String,
    /// The session's tenant, carried into generated MCP configurations.
    tenant: Option<String>,
    /// The connection driver. It finishes only when the link to PostgreSQL
    /// is gone for good, and this connection is never re-established, so a
    /// long-running transport takes it and stops when it ends.
    driver: Option<JoinHandle<()>>,
}

impl Catalog {
    /// Connect to PostgreSQL and, when `tenant` is set, apply it as the
    /// session's `pgokf.tenant` so tenant row-level security is enforced.
    ///
    /// `force_tls` (the `--tls` flag) requires an encrypted link; TLS is also
    /// negotiated for an `sslmode=require` connection URL. The connection
    /// driver task is spawned by the shared helper; [`Catalog::take_driver`]
    /// hands its handle to a transport that outlives one client.
    ///
    /// # Errors
    ///
    /// Returns an error if the connection or the tenant scoping fails.
    pub async fn connect(
        database_url: &str,
        tenant: Option<&str>,
        force_tls: bool,
    ) -> Result<Self> {
        let (client, driver) = pgokf_pgconn::connect(database_url, force_tls)
            .await
            .context("connecting to PostgreSQL")?;

        if let Some(tenant) = tenant {
            pgokf_pgconn::set_tenant(&client, tenant).await?;
        }
        let (build_client, build_driver) = pgokf_pgconn::connect(database_url, force_tls)
            .await
            .context("connecting to PostgreSQL for plugin builds")?;
        if let Some(tenant) = tenant {
            pgokf_pgconn::set_tenant(&build_client, tenant).await?;
        }
        let database_name: String = client
            .query_one("SELECT current_database()", &[])
            .await
            .context("reading the database name")?
            .try_get(0)?;

        Ok(Self {
            client,
            build: Mutex::new(build_client),
            build_driver: Some(build_driver),
            writer: None,
            writer_driver: None,
            database_name,
            tenant: tenant.map(str::to_owned),
            driver: Some(driver),
        })
    }

    /// Add the writer connection the `writer` and `admin` roles need, on
    /// `writer_url` - a `pgokf_writer`-capable role - scoped to the same
    /// tenant as the reader.
    ///
    /// # Errors
    ///
    /// Returns an error if the connection or the tenant scoping fails.
    pub async fn with_writer(mut self, writer_url: &str, force_tls: bool) -> Result<Self> {
        let (client, driver) = pgokf_pgconn::connect(writer_url, force_tls)
            .await
            .context("connecting to PostgreSQL as the writer")?;
        if let Some(tenant) = &self.tenant {
            pgokf_pgconn::set_tenant(&client, tenant).await?;
        }
        let writer = WriterConn::new(client);
        // Bounded on every transport, stdio included: the lock a write takes
        // is shared with processes this one knows nothing about, and a write
        // that waits for ever on one of them is worse than one that fails.
        writer.set_lock_timeout(WRITER_LOCK_WAIT_MS).await?;
        writer.set_statement_timeout(WRITER_STATEMENT_MS).await?;
        self.writer = Some(writer);
        self.writer_driver = Some(driver);
        Ok(self)
    }

    /// The writer connection, or why there is none to use.
    fn writer(&self) -> Result<&WriterConn> {
        self.writer.as_ref().context(
            "this MCP server has no writer connection, so it cannot change the catalog: start \
             it with --writer-url (OKF_PG_WRITER_URL) naming a pgokf_writer-capable role",
        )
    }

    /// Take the connection driver, to wait on it.
    ///
    /// It finishes only when the link to PostgreSQL is gone, and nothing
    /// re-establishes it, so a server that means to keep running takes this
    /// and stops when it ends rather than answering every later call with
    /// the same failure. A second call returns `None`. The build
    /// connection's driver is folded in: the returned task ends when either
    /// link does.
    pub fn take_driver(&mut self) -> Option<JoinHandle<()>> {
        match (self.driver.take(), self.build_driver.take()) {
            (Some(reader), Some(build)) => Some(tokio::spawn(async move {
                tokio::select! {
                    _ = reader => {}
                    _ = build => {}
                }
            })),
            (reader, None) => reader,
            (None, build) => build,
        }
    }

    /// Take the writer connection's driver, to wait on it beside the
    /// reader's. A second call returns `None`.
    pub fn take_writer_driver(&mut self) -> Option<JoinHandle<()>> {
        self.writer_driver.take()
    }

    /// Bound every statement this session runs.
    ///
    /// One connection is shared by every in-flight request, so a query that
    /// outlives the caller's patience would otherwise keep running and hold
    /// up everything pipelined behind it. Cancelling the future does not
    /// cancel the query; a server-side timeout does.
    ///
    /// # Errors
    ///
    /// The `SET` failing.
    pub async fn set_statement_timeout(&self, timeout: Duration) -> Result<()> {
        let millis = i32::try_from(timeout.as_millis()).unwrap_or(i32::MAX);
        self.client
            .execute(
                "SELECT set_config('statement_timeout', $1, false)",
                &[&millis.to_string()],
            )
            .await
            .context("setting the statement timeout")?;
        self.build
            .lock()
            .await
            .execute(
                "SELECT set_config('statement_timeout', $1, false)",
                &[&millis.to_string()],
            )
            .await
            .context("setting the build connection's statement timeout")?;
        // The writer is one shared, serialized connection too: an unbounded
        // statement on it would hold up every other write.
        if let Some(writer) = &self.writer {
            writer.set_statement_timeout(millis).await?;
        }
        Ok(())
    }

    /// Whether the catalog still answers.
    ///
    /// The connection is never re-established, so a link that has died stays
    /// dead: a health probe that reports this lets a supervisor restart the
    /// process instead of leaving it up and failing every call.
    ///
    /// # Errors
    ///
    /// The query failing, which means the link is gone.
    pub async fn ping(&self) -> Result<()> {
        self.client
            .query_one("SELECT 1", &[])
            .await
            .context("the catalog did not answer")?;
        Ok(())
    }

    /// The MCP `tools/list` payload: the catalog tools this server exposes,
    /// each with a JSON-Schema description of its arguments.
    #[must_use]
    pub fn tool_definitions() -> Value {
        let mut tools = Self::catalog_tool_definitions();
        if let Value::Array(items) = &mut tools {
            for more in [Self::plugin_tool_definitions(), write::tool_definitions()] {
                if let Value::Array(more) = more {
                    items.extend(more);
                }
            }
        }
        tools
    }

    /// The search, graph, and retrieval tools.
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

    /// Dispatch one `tools/call` to the matching catalog query, returning the
    /// tool's JSON result data (a JSON array of rows).
    ///
    /// # Errors
    ///
    /// Returns an error for an unknown tool, an argument that is missing or the
    /// wrong type, or a database failure. The caller renders the error as an
    /// MCP `isError` tool result.
    pub async fn call_tool(&self, name: &str, arguments: &Value, actor: &str) -> Result<Value> {
        if write::is_write_tool(name) {
            return write::call(self.writer()?, name, arguments, actor).await;
        }
        match name {
            // A read, so it answers on an endpoint that holds no writer.
            "list_bundles" => write::list_bundles(&self.client, arguments).await,
            "concept_search" => self.concept_search(arguments).await,
            "find_similar" => self.find_similar(arguments).await,
            "concept_neighbors" => self.concept_neighbors(arguments).await,
            "get_concept" => self.get_concept(arguments).await,
            "get_skill" => self.get_skill(arguments).await,
            "list_plugin_targets" => Ok(Self::list_plugin_targets()),
            "build_workspace_plugin" => self.build_workspace_plugin(arguments).await,
            "check_workspace_plugin_freshness" => {
                self.check_workspace_plugin_freshness(arguments).await
            }
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

    /// `check_workspace_plugin_freshness`: the lockfile is compared with the
    /// live catalog in one read; the answer is the plugin's status with
    /// per-bundle reasons.
    async fn check_workspace_plugin_freshness(&self, args: &Value) -> Result<Value> {
        let lock = require_str(args, "lock")?;
        let check = pgokf_workspace::check_plugin_freshness(&self.client, lock).await?;
        serde_json::to_value(check).context("rendering the freshness check")
    }

    fn list_plugin_targets() -> Value {
        Value::Array(
            Profile::all()
                .iter()
                .map(|p| {
                    json!({
                        "id": p.id,
                        "label": p.label,
                        "kind": p.shape.id(),
                        "kind_label": p.shape.label(),
                        "shape": format!("{:?}", p.shape),
                        "root": p.root,
                        "documented_at": p.source,
                        "verified": p.verified,
                        "notes": p.notes,
                        // What `mcp_url` would do for this target: where the
                        // entry lands, and how the token reaches it. Absent
                        // when the target has no MCP configuration at all.
                        "mcp": p.mcp.map(|spec| {
                            let (remote_path, auto_loaded) = spec.location(true);
                            json!({
                                "path": spec.path,
                                "auto_loaded": spec.auto_loaded,
                                "remote": spec.remote.map(|remote| json!({
                                    "path": remote_path,
                                    "auto_loaded": auto_loaded,
                                    "merge_into": spec.merge_target(true),
                                    "url_key": remote.url_key,
                                    "type": remote.type_word,
                                    "token": remote.token_ref.id(),
                                    "token_env": (remote.token_ref != TokenRef::Forbidden)
                                        .then_some(TOKEN_ENV),
                                    "documented_at": remote.source,
                                })),
                            })
                        }),
                    })
                })
                .collect(),
        )
    }

    /// The selection the tool arguments describe.
    fn selection_from_args(args: &Value) -> Result<Selection> {
        let picks = ConceptRef::parse_list(
            &opt_string_vec(args, "picks")?
                .unwrap_or_default()
                .join("\n"),
        )?;
        let seeds = ConceptRef::parse_list(
            &opt_string_vec(args, "seeds")?
                .unwrap_or_default()
                .join("\n"),
        )?;
        let stale_policy = match opt_str(args, "stale_policy") {
            None => StalePolicy::Warn,
            Some(id) => StalePolicy::parse(id)
                .ok_or_else(|| anyhow!("argument 'stale_policy' must be 'warn' or 'exclude'"))?,
        };
        let direction = match opt_str(args, "direction") {
            None => Direction::Outbound,
            Some(id) => Direction::parse(id).ok_or_else(|| {
                anyhow!("argument 'direction' must be 'outbound', 'inbound', or 'both'")
            })?,
        };
        let hops = match opt_i64(args, "hops")? {
            None => None,
            Some(n)
                if usize::try_from(n)
                    .is_ok_and(|n| (1..=pgokf_workspace::MAX_HOPS).contains(&n)) =>
            {
                usize::try_from(n).ok()
            }
            Some(_) => bail!(
                "argument 'hops' must be between 1 and {}",
                pgokf_workspace::MAX_HOPS
            ),
        };
        Ok(Selection {
            picks,
            stale_policy,
            seeds,
            relation_types: opt_string_vec(args, "relation_types")?.unwrap_or_default(),
            direction,
            hops,
            require_closure: args
                .get("require_closure")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            all: args.get("all").and_then(Value::as_bool).unwrap_or(false),
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
            harness: harness_from_args(args, target)?,
            name: opt_str(args, "name").unwrap_or("okf-knowledge").to_owned(),
            title: opt_str(args, "title").map(str::to_owned),
            catalog_name: self.database_name.clone(),
            base_model: opt_str(args, "base_model").map(str::to_owned),
            components,
            mcp_command: opt_str(args, "mcp_command").map(str::to_owned),
            mcp_url: opt_str(args, "mcp_url").map(str::to_owned),
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
        // The build holds one repeatable-read snapshot for its whole run, so
        // it takes the dedicated connection: the shared client pipelines
        // concurrent callers' statements, and a transaction cannot live
        // there. The lock serializes builds.
        let plugin = {
            let mut build = self.build.lock().await;
            match pgokf_workspace::build_in_transaction(&mut build, &options, &selection).await {
                Ok(plugin) => plugin,
                Err(error) => {
                    // A stale-policy or closure refusal is a normal answer,
                    // returned with the enumerations, not an error string.
                    if let Some(refusal) = error
                        .chain()
                        .find_map(|cause| cause.downcast_ref::<pgokf_workspace::BuildRefusal>())
                    {
                        return Ok(refusal.to_json());
                    }
                    return Err(error);
                }
            }
        };

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
                "freshness": c.freshness.state,
            })).collect::<Vec<_>>(),
            "files": plugin.files.iter().map(|f| json!({
                "path": f.path,
                "bytes": f.bytes.len(),
                "sha256": f.sha256,
                "executable": f.executable,
            })).collect::<Vec<_>>(),
        });
        if !plugin.warnings.is_empty() {
            result["warnings"] = serde_json::to_value(&plugin.warnings).unwrap_or_default();
        }
        if !plugin.excluded.is_empty() {
            result["excluded"] = serde_json::to_value(&plugin.excluded).unwrap_or_default();
        }
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
                    "the tree is {} bytes; narrow the selection (fewer concepts, or fewer \
                     components), or on a local transport pass output_dir to write it instead \
                     of returning it inline",
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

/// The custom harness a `target: custom` build describes (`None` for a
/// registry target, whose layout is documented and needs no description).
fn harness_from_args(args: &Value, target: Target) -> Result<Option<CustomHarness>> {
    if target != Target::Custom {
        return Ok(None);
    }
    let harness = args
        .get("harness")
        .filter(|h| h.is_object())
        .ok_or_else(|| anyhow!("target 'custom' needs a 'harness' object: label, kind, and for a skills package skills_dir"))?;
    let label = require_str(harness, "label")?;
    let kind = match opt_str(harness, "kind") {
        None => Shape::Skills,
        Some(id) => Shape::parse(id).ok_or_else(|| {
            anyhow!("unknown harness kind '{id}'; use agent-plugin, skills, instruction-file, prompt-bundle, or generic")
        })?,
    };
    CustomHarness::new(label, kind, opt_str(harness, "skills_dir")).map(Some)
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
    fn a_custom_target_needs_a_valid_harness_and_registry_targets_ignore_one() {
        // Arrange
        let described = json!({
            "target": "custom",
            "harness": { "label": "Acme Agent", "kind": "skills", "skills_dir": ".acme/skills" }
        });
        let defaulted = json!({ "target": "custom", "harness": { "label": "Acme" } });
        let bad_kind =
            json!({ "target": "custom", "harness": { "label": "Acme", "kind": "plugin" } });
        let registry = json!({ "target": "codex", "harness": { "label": "ignored" } });

        // Act
        let harness = harness_from_args(&described, Target::Custom)
            .expect("valid")
            .expect("described");
        let missing_dir = harness_from_args(&defaulted, Target::Custom);
        let unknown_kind = harness_from_args(&bad_kind, Target::Custom);
        let none = harness_from_args(&registry, Target::Codex).expect("ok");
        let absent = harness_from_args(&json!({ "target": "custom" }), Target::Custom);

        // Assert
        assert_eq!(harness.label, "Acme Agent");
        assert_eq!(harness.skills_dir.as_deref(), Some(".acme/skills"));
        assert!(
            missing_dir
                .expect_err("skills needs a directory")
                .to_string()
                .contains("reads Agent Skills")
        );
        assert!(
            unknown_kind
                .expect_err("bad kind")
                .to_string()
                .contains("unknown harness kind")
        );
        assert!(none.is_none());
        assert!(
            absent
                .expect_err("no harness")
                .to_string()
                .contains("'harness' object")
        );
    }

    #[test]
    fn selections_carry_the_everything_scope() {
        // Arrange / Act
        let all = Catalog::selection_from_args(&json!({ "all": true, "types": ["Runbook"] }))
            .expect("valid");
        let plain = Catalog::selection_from_args(&json!({ "types": ["Runbook"] })).expect("valid");

        // Assert
        assert!(all.all && all.has_filters());
        assert!(!plain.all);
    }

    #[test]
    fn tool_definitions_lists_the_catalog_plugin_and_writing_tools() {
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
                "check_workspace_plugin_freshness",
                "list_bundles",
                "put_document",
                "delete_document",
                "create_content_bundle",
                "refresh_bundle",
                "set_bundle_state",
            ],
        );
    }

    #[test]
    fn the_build_selection_parses_the_stale_policy_and_closure_arguments() {
        // Arrange / Act
        let full = Catalog::selection_from_args(&json!({
            "stale_policy": "exclude",
            "seeds": ["1:runbooks/a", "2:b"],
            "relation_types": ["docs:references"],
            "direction": "both",
            "hops": 3,
            "require_closure": true,
        }))
        .expect("valid");
        let plain = Catalog::selection_from_args(&json!({})).expect("defaults");

        // Assert
        assert_eq!(full.stale_policy, StalePolicy::Exclude);
        assert_eq!(
            full.seeds
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>(),
            ["1:runbooks/a", "2:b"]
        );
        assert_eq!(full.relation_types, ["docs:references"]);
        assert_eq!(full.direction, Direction::Both);
        assert_eq!(full.hops, Some(3));
        assert!(full.require_closure);
        assert_eq!(plain.stale_policy, StalePolicy::Warn);
        assert!(plain.seeds.is_empty() && !plain.require_closure);
        assert_eq!(plain.direction, Direction::Outbound);

        // Bad values are named.
        for args in [
            json!({"stale_policy": "drop"}),
            json!({"direction": "up"}),
            json!({"hops": 0}),
            json!({"hops": 99}),
            json!({"seeds": ["no-colon"]}),
        ] {
            assert!(
                Catalog::selection_from_args(&args).is_err(),
                "{args} should fail"
            );
        }
    }
}
