// SPDX-License-Identifier: AGPL-3.0-only
//! The tools that change the catalog, for the `writer` and `admin` roles.
//!
//! They need a connection of their own - a `pgokf_writer`-capable role,
//! given with `--writer-url` - because everything else this server does it
//! does as a reader. Without one the tools are still declared, so an agent
//! discovers them and is told plainly that this endpoint does not write,
//! rather than finding a tool that silently is not there.
//!
//! Two rules hold whatever writes:
//!
//! - **A contribution arrives unverified.** Whatever the incoming document
//!   claims under `verified` is set aside, visibly, with who set it aside and
//!   why ([`Document::contribute_new`] / [`Document::contribute_edit`], the
//!   same code the web UI's upload and edit paths run). A verification is
//!   granted by an approver reviewing the document; it is never something a
//!   contributor - a person or an agent - can type into one. `generated`
//!   names the token as `agent:<name>`, so the trust tier the extension
//!   derives says an agent produced it.
//! - **A write is a full snapshot.** `pgokf.register_bundle_content` replaces
//!   a content bundle with exactly the files it is given, so one document is
//!   written by reading the bundle, changing the one entry, and sending all
//!   of it back. That read-modify-write is serialized here, and refused
//!   outright when the catalog does not keep document sources, because the
//!   files could not be read back to send.

use anyhow::{Context, Result, anyhow, bail};
use pgokf_companion::documents::{Document, now_iso};
use serde_json::{Value, json};
use tokio::sync::Mutex;
use tokio_postgres::Client;

/// A `pgokf_writer` connection, and the lock that keeps two snapshot
/// rewrites of the same bundle from interleaving and losing one of them.
pub(crate) struct WriterConn {
    client: Client,
    one_at_a_time: Mutex<()>,
}

impl WriterConn {
    pub(crate) fn new(client: Client) -> Self {
        Self {
            client,
            one_at_a_time: Mutex::new(()),
        }
    }
}

/// Whether `tool` is one of the tools defined here.
pub(crate) fn is_write_tool(tool: &str) -> bool {
    matches!(
        tool,
        "list_bundles"
            | "put_document"
            | "delete_document"
            | "create_content_bundle"
            | "refresh_bundle"
            | "set_bundle_state"
    )
}

/// The writing and administering tools, as `tools/list` declares them.
pub(crate) fn tool_definitions() -> Value {
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

/// One bundle, as the write tools resolve it.
#[derive(Debug)]
struct Bundle {
    id: i64,
    name: String,
    source_type: String,
}

impl Bundle {
    /// That this is a content bundle, which is the only kind this API writes.
    fn content(&self) -> Result<&Self> {
        if self.source_type == "content" {
            return Ok(self);
        }
        bail!(
            "bundle {} ({}) is a {} bundle: its documents come from the directory the database \
             server reads, so they are changed there and picked up by refresh_bundle, not \
             written through this API",
            self.id,
            self.name,
            self.source_type
        )
    }
}

/// Dispatch one write tool. `actor` is the OKF actor recorded as the
/// producer of anything written.
pub(crate) async fn call(
    writer: &WriterConn,
    tool: &str,
    args: &Value,
    actor: &str,
) -> Result<Value> {
    match tool {
        "list_bundles" => list_bundles(&writer.client, args).await,
        "put_document" => put_document(writer, args, actor).await,
        "delete_document" => delete_document(writer, args).await,
        "create_content_bundle" => create_content_bundle(&writer.client, args).await,
        "refresh_bundle" => refresh_bundle(&writer.client, args).await,
        "set_bundle_state" => set_bundle_state(&writer.client, args).await,
        other => bail!("unknown tool '{other}'"),
    }
}

async fn list_bundles(client: &Client, args: &Value) -> Result<Value> {
    let include_retired = args
        .get("include_retired")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let row = client
        .query_one(
            "SELECT coalesce(jsonb_agg(to_jsonb(t) ORDER BY t.id), '[]'::jsonb) FROM (
                 SELECT b.id, coalesce(b.name, b.path) AS name, b.source_type,
                        b.enabled, (b.retired_at IS NOT NULL) AS retired, b.file_count
                 FROM pgokf.bundles b
                 WHERE $1 OR b.retired_at IS NULL
             ) t",
            &[&include_retired],
        )
        .await
        .context("listing bundles")?;
    Ok(row.get(0))
}

/// The bundle an argument names, by id or by name.
async fn resolve(client: &Client, args: &Value) -> Result<Bundle> {
    let by_id = match args.get("bundle_id") {
        None | Some(Value::Null) => None,
        Some(value) => Some(
            value
                .as_i64()
                .ok_or_else(|| anyhow!("argument 'bundle_id' must be an integer"))?,
        ),
    };
    let by_name = args.get("bundle").and_then(Value::as_str);
    if by_id.is_none() && by_name.is_none() {
        bail!("name the bundle: pass 'bundle' (its name) or 'bundle_id'");
    }
    let row = client
        .query_opt(
            "SELECT b.id, coalesce(b.name, b.path), b.source_type
             FROM pgokf.bundles b
             WHERE ($1::bigint IS NOT NULL AND b.id = $1)
                OR ($2::text IS NOT NULL AND b.name = $2)",
            &[&by_id, &by_name],
        )
        .await
        .context("looking the bundle up")?;
    let row = row.ok_or_else(|| match (by_id, by_name) {
        (Some(id), _) => anyhow!("no bundle has id {id}"),
        (_, Some(name)) => anyhow!("no bundle is named '{name}'; list_bundles shows them"),
        _ => anyhow!("no such bundle"),
    })?;
    Ok(Bundle {
        id: row.try_get(0)?,
        name: row.try_get(1)?,
        source_type: row.try_get(2)?,
    })
}

/// That the catalog keeps document sources. Without them a bundle cannot be
/// read back to be written whole, so a write would drop every document it
/// did not carry.
async fn ensure_sources(client: &Client) -> Result<()> {
    let row = client
        .query_one(
            "SELECT coalesce((pgokf.get_config() ->> 'store_source')::boolean, false)",
            &[],
        )
        .await
        .context("reading the catalog's configuration")?;
    if row.try_get::<_, bool>(0)? {
        return Ok(());
    }
    bail!(
        "this catalog does not keep document sources (store_source is off), so a bundle cannot \
         be read back to be written whole. An admin turns it on with \
         pgokf.set_config('store_source', 'true') and refreshes the bundles"
    )
}

/// Every file of a content bundle as it stands, ready to be sent back.
async fn snapshot(client: &Client, bundle_id: i64) -> Result<Vec<(String, Vec<u8>)>> {
    let rows = client
        .query(
            "SELECT c.path,
                    coalesce(sk.skill_md, sc.exact_bytes, rd.exact_bytes, s.raw_content)
             FROM pgokf.concepts c
             LEFT JOIN pgokf.concept_source s
                    ON s.bundle_id = c.bundle_id AND s.concept_id = c.id
             LEFT JOIN pgokf.skills sk ON sk.bundle_id = c.bundle_id AND sk.concept_id = c.id
             LEFT JOIN pgokf.scripts sc ON sc.bundle_id = c.bundle_id AND sc.concept_id = c.id
             LEFT JOIN pgokf.reference_documents rd
                    ON rd.bundle_id = c.bundle_id AND rd.concept_id = c.id
             WHERE c.bundle_id = $1
             ORDER BY c.path",
            &[&bundle_id],
        )
        .await
        .context("reading the bundle's documents")?;
    let mut files = Vec::with_capacity(rows.len());
    for row in &rows {
        let path: String = row.try_get(0)?;
        let bytes: Option<Vec<u8>> = row.try_get(1)?;
        // A document whose bytes the catalog does not hold cannot be sent
        // back, and sending the rest would delete it. Refuse the write.
        let bytes = bytes.ok_or_else(|| {
            anyhow!(
                "the catalog holds no source for {path}, so the bundle cannot be written whole \
                 without losing it; refresh the bundle with store_source on first"
            )
        })?;
        files.push((path, bytes));
    }
    Ok(files)
}

/// Send a whole content bundle back, replacing what is there.
async fn resync(client: &Client, name: &str, files: &[(String, Vec<u8>)]) -> Result<Value> {
    let paths: Vec<&str> = files.iter().map(|(path, _)| path.as_str()).collect();
    let contents: Vec<&[u8]> = files.iter().map(|(_, bytes)| bytes.as_slice()).collect();
    let row = client
        .query_one(
            "SELECT to_jsonb(r) FROM pgokf.register_bundle_content($1, $2, $3, '{}'::jsonb) r",
            &[&name, &paths, &contents],
        )
        .await
        .context("writing the bundle")?;
    Ok(row.get(0))
}

async fn put_document(writer: &WriterConn, args: &Value, actor: &str) -> Result<Value> {
    let path = require_path(args)?;
    let text = args
        .get("text")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("missing required string argument 'text'"))?;

    // One rewrite at a time: two callers reading the same bundle and each
    // sending back their own snapshot would lose one of the two writes.
    let _one_at_a_time = writer.one_at_a_time.lock().await;
    let client = &writer.client;
    ensure_sources(client).await?;
    let bundle = resolve(client, args).await?;
    bundle.content()?;

    let mut files = snapshot(client, bundle.id).await?;
    let existing = files.iter().position(|(stored, _)| *stored == path);
    let stored = existing
        .and_then(|at| std::str::from_utf8(&files[at].1).ok())
        .and_then(|text| Document::parse(text).ok());

    let mut document = Document::parse(text).map_err(|why| anyhow!("{path}: {why}"))?;
    let now = now_iso();
    let set_aside = match &stored {
        Some(stored) => document.contribute_edit(actor, &now, Some(stored)),
        None => document.contribute_new(actor, &now),
    };
    document
        .validate(&path)
        .map_err(|why| anyhow!("{path}: {why}"))?;
    let bytes = document.render().into_bytes();

    match existing {
        Some(at) => files[at].1 = bytes,
        None => files.push((path.clone(), bytes)),
    }
    files.sort_by(|(a, _), (b, _)| a.cmp(b));
    let outcome = resync(client, &bundle.name, &files).await?;

    Ok(json!({
        "bundle_id": bundle.id,
        "bundle": bundle.name,
        "path": path,
        "replaced": existing.is_some(),
        "generated_by": actor,
        "verifications_set_aside": set_aside,
        "sync": outcome,
        "note": "Written as machine-generated. A person reviews it in the catalog's web UI; \
                 until then its trust tier is not human-reviewed.",
    }))
}

async fn delete_document(writer: &WriterConn, args: &Value) -> Result<Value> {
    let path = require_path(args)?;

    let _one_at_a_time = writer.one_at_a_time.lock().await;
    let client = &writer.client;
    ensure_sources(client).await?;
    let bundle = resolve(client, args).await?;
    bundle.content()?;

    let mut files = snapshot(client, bundle.id).await?;
    let before = files.len();
    files.retain(|(stored, _)| *stored != path);
    if files.len() == before {
        bail!("{} holds no document at {path}", bundle.name);
    }
    let outcome = resync(client, &bundle.name, &files).await?;

    Ok(json!({
        "bundle_id": bundle.id,
        "bundle": bundle.name,
        "path": path,
        "deleted": true,
        "sync": outcome,
    }))
}

async fn create_content_bundle(client: &Client, args: &Value) -> Result<Value> {
    let name = args
        .get("name")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .ok_or_else(|| anyhow!("missing required string argument 'name'"))?;
    // A content bundle is keyed on its name, so registering an existing one
    // with no files would empty it. Refuse rather than resync.
    let taken: bool = client
        .query_one(
            "SELECT EXISTS (SELECT 1 FROM pgokf.bundles WHERE name = $1 OR path = 'content:' || $1)",
            &[&name],
        )
        .await
        .context("checking the bundle name")?
        .try_get(0)?;
    if taken {
        bail!("a bundle named '{name}' already exists");
    }
    let outcome = resync(client, name, &[]).await?;
    Ok(json!({ "created": name, "sync": outcome }))
}

async fn refresh_bundle(client: &Client, args: &Value) -> Result<Value> {
    let bundle = resolve(client, args).await?;
    if bundle.source_type == "content" {
        bail!(
            "bundle {} ({}) is a content bundle: it has no source to re-read, and its documents \
             are written with put_document",
            bundle.id,
            bundle.name
        );
    }
    let row = client
        .query_one(
            "SELECT to_jsonb(r) FROM pgokf.refresh_bundle($1) r",
            &[&bundle.id],
        )
        .await
        .context("refreshing the bundle")?;
    Ok(json!({ "bundle_id": bundle.id, "bundle": bundle.name, "sync": row.get::<_, Value>(0) }))
}

async fn set_bundle_state(client: &Client, args: &Value) -> Result<Value> {
    let state = args
        .get("state")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("missing required string argument 'state'"))?;
    let bundle = resolve(client, args).await?;
    let sql = match state {
        "enabled" => "SELECT pgokf.set_bundle_enabled($1, true)",
        "disabled" => "SELECT pgokf.set_bundle_enabled($1, false)",
        "retired" => "SELECT pgokf.retire_bundle($1)",
        "active" => "SELECT pgokf.unretire_bundle($1)",
        other => bail!("'state' is enabled, disabled, retired, or active, not '{other}'"),
    };
    client
        .execute(sql, &[&bundle.id])
        .await
        .with_context(|| format!("setting bundle {} to {state}", bundle.id))?;
    Ok(json!({ "bundle_id": bundle.id, "bundle": bundle.name, "state": state }))
}

/// The `path` argument, which every document tool needs.
fn require_path(args: &Value) -> Result<String> {
    let path = args
        .get("path")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|path| !path.is_empty())
        .ok_or_else(|| anyhow!("missing required string argument 'path'"))?;
    Ok(path.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_declared_write_tool_is_one_this_module_dispatches() {
        // Arrange
        let Value::Array(tools) = tool_definitions() else {
            panic!("the definitions are an array");
        };

        // Act / Assert
        for tool in &tools {
            let name = tool["name"].as_str().expect("a name");
            assert!(is_write_tool(name), "{name} is declared but not dispatched");
        }
        assert_eq!(tools.len(), 6);
    }

    #[test]
    fn a_filesystem_bundle_is_not_written_through_this_api() {
        // Arrange
        let content = Bundle {
            id: 1,
            name: "team-docs".to_owned(),
            source_type: "content".to_owned(),
        };
        let directory = Bundle {
            id: 2,
            name: "handbook".to_owned(),
            source_type: "filesystem".to_owned(),
        };

        // Act / Assert
        assert!(content.content().is_ok());
        let refused = directory.content().expect_err("refused");
        assert!(refused.to_string().contains("refresh_bundle"), "{refused}");
    }

    #[test]
    fn a_path_argument_is_required_and_trimmed() {
        // Arrange
        let given = json!({"path": "  runbooks/failover.md "});
        let blank = json!({"path": "   "});

        // Act / Assert
        assert_eq!(
            require_path(&given).expect("a path"),
            "runbooks/failover.md"
        );
        assert!(require_path(&blank).is_err());
        assert!(require_path(&json!({})).is_err());
    }
}
