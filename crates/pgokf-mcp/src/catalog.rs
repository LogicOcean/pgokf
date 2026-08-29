//! Catalog access and MCP tool definitions.
//!
//! Each MCP tool is backed by a single query against the shipped `pgokf` public
//! surface. Every query aggregates its result rows into one `jsonb` array with
//! `jsonb_agg(to_jsonb(...))`, so the server hands MCP a faithful JSON view of
//! exactly what the SQL functions return, with no per-column marshalling.

use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Value, json};
use tokio_postgres::types::ToSql;
use tokio_postgres::{Client, NoTls};

/// Default `limit` for `concept_search` when the caller omits it.
const DEFAULT_SEARCH_LIMIT: i32 = 20;
/// Default `limit` for `find_similar` when the caller omits it.
const DEFAULT_SIMILAR_LIMIT: i32 = 10;
/// Default `max_hops` for `concept_neighbors` when the caller omits it.
const DEFAULT_MAX_HOPS: i32 = 2;

/// A live catalog connection, optionally scoped to one tenant.
pub struct Catalog {
    client: Client,
}

impl Catalog {
    /// Connect to PostgreSQL and, when `tenant` is set, apply it as the
    /// session's `pgokf.tenant` so tenant row-level security is enforced.
    ///
    /// # Errors
    ///
    /// Returns an error if the connection or the tenant scoping fails.
    pub async fn connect(database_url: &str, tenant: Option<&str>) -> Result<Self> {
        let (client, connection) = tokio_postgres::connect(database_url, NoTls)
            .await
            .context("connecting to PostgreSQL")?;
        tokio::spawn(async move {
            if let Err(error) = connection.await {
                eprintln!("pgokf-mcp: PostgreSQL connection error: {error}");
            }
        });

        if let Some(tenant) = tenant {
            client
                .execute("SELECT set_config('pgokf.tenant', $1, false)", &[&tenant])
                .await
                .context("failed to set pgokf.tenant")?;
        }

        Ok(Self { client })
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
    fn tool_definitions_lists_the_four_catalog_tools() {
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
                "get_concept"
            ],
        );
    }
}
