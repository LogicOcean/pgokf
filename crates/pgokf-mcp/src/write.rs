// SPDX-License-Identifier: AGPL-3.0-only
//! The tools that change the catalog, for the `writer` and `admin` roles.
//!
//! They need a connection of their own - a `pgokf_writer`-capable role,
//! given with `--writer-url` - because everything else this server does it
//! does as a reader. Without one the tools are still declared, so an agent
//! discovers them and is told plainly that this endpoint does not write,
//! rather than finding a tool that silently is not there.
//!
//! Four rules hold whatever writes:
//!
//! - **A contribution arrives unverified.** Whatever the incoming document
//!   claims under `verified` is set aside, visibly, with who set it aside and
//!   why ([`Document::contribute_new`] / [`Document::contribute_edit`], the
//!   same code the web UI's upload and edit paths run), and it may not put
//!   another person's name to its origin. A verification is granted by an
//!   approver reviewing the document; it is never something a contributor -
//!   a person or an agent - can type into one. `generated` names the token
//!   as `agent:<name>`, so the trust tier the extension derives says an
//!   agent produced it.
//! - **A write is a full snapshot.** `pgokf.register_bundle_content` replaces
//!   a content bundle with exactly the files it is given, so one document is
//!   written by reading the bundle, changing the one entry, and sending all
//!   of it back.
//! - **That snapshot is serialized across every writer**, not merely within
//!   this process: the read and the write run in one transaction holding a
//!   PostgreSQL advisory lock on the bundle's name, so a second server, or
//!   the web UI, cannot interleave and drop what the other wrote.
//! - **A write that could not put the bundle back as it found it is
//!   refused.** The snapshot is built from what the catalog stores, so a
//!   bundle carrying anything the catalog does not store the bytes of - an
//!   `index.md`, a `log.md` - is refused rather than written back without
//!   them. So is one too large to hold in memory, and one whose name does
//!   not resolve to the row that was read.

use anyhow::{Context, Result, anyhow, bail};
use pgokf_companion::documents::{Document, now_iso};
use serde_json::{Value, json};
use tokio::sync::Mutex;
use tokio_postgres::{Client, Transaction};

/// The largest bundle a single document write will rewrite. A write reads
/// the whole bundle into memory and sends it back, so the ceiling is what
/// this process is willing to hold at once - far below the catalog's own
/// `max_bundle_files`.
const MAX_SNAPSHOT_FILES: i32 = 5_000;
/// The same ceiling in bytes.
const MAX_SNAPSHOT_BYTES: i64 = 64 * 1024 * 1024;
/// How much of a caller-supplied value an error message repeats back.
const ECHO_MAX: usize = 96;

/// A `pgokf_writer` connection. The client is behind a lock because a write
/// runs in a transaction, which needs it exclusively - and because one
/// connection serves every request.
pub(crate) struct WriterConn {
    client: Mutex<Client>,
}

impl WriterConn {
    pub(crate) fn new(client: Client) -> Self {
        Self {
            client: Mutex::new(client),
        }
    }

    /// Bound every statement this connection runs, as the reader's are
    /// bounded: cancelling a request does not cancel its query, and this
    /// connection is serialized, so one unbounded statement would hold up
    /// every other write.
    ///
    /// # Errors
    ///
    /// The `SET` failing.
    pub(crate) async fn set_statement_timeout(&self, millis: i32) -> Result<()> {
        self.client
            .lock()
            .await
            .execute(
                "SELECT set_config('statement_timeout', $1, false)",
                &[&millis.to_string()],
            )
            .await
            .context("setting the writer's statement timeout")?;
        Ok(())
    }

    /// Bound how long a write waits for another writer of the same bundle -
    /// the web UI, or another instance of this server - so a wedged writer
    /// holding the lock cannot hang this one for ever. Applied on every
    /// transport, stdio included, since the lock is shared with processes
    /// this one knows nothing about.
    ///
    /// # Errors
    ///
    /// The `SET` failing.
    pub(crate) async fn set_lock_timeout(&self, millis: i32) -> Result<()> {
        self.client
            .lock()
            .await
            .execute(
                "SELECT set_config('lock_timeout', $1, false)",
                &[&millis.to_string()],
            )
            .await
            .context("setting the writer's lock timeout")?;
        Ok(())
    }
}

/// Whether `tool` is one of the tools that change the catalog. `list_bundles`
/// is not among them: it only reads, so it is served by the reader and works
/// on an endpoint with no writer connection at all.
pub(crate) fn is_write_tool(tool: &str) -> bool {
    matches!(
        tool,
        "put_document"
            | "delete_document"
            | "create_content_bundle"
            | "refresh_bundle"
            | "set_bundle_state"
    )
}

/// As much of a caller-supplied value as an error message repeats.
fn echo(value: &str) -> String {
    let mut out: String = value.chars().take(ECHO_MAX).collect();
    if out.chars().count() < value.chars().count() {
        out.push('…');
    }
    out
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
    file_count: i32,
    retired: bool,
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
            echo(&self.name),
            self.source_type
        )
    }
}

/// Dispatch one tool that changes the catalog. `actor` is the OKF actor
/// recorded as the producer of anything written.
pub(crate) async fn call(
    writer: &WriterConn,
    tool: &str,
    args: &Value,
    actor: &str,
) -> Result<Value> {
    // One transaction per call, so the read a write is based on and the
    // write itself cannot be separated by anyone else's.
    let mut client = writer.client.lock().await;
    let tx = client
        .transaction()
        .await
        .context("starting the write transaction")?;
    let outcome = match tool {
        "put_document" => put_document(&tx, args, actor).await,
        "delete_document" => delete_document(&tx, args).await,
        "create_content_bundle" => create_content_bundle(&tx, args).await,
        "refresh_bundle" => refresh_bundle(&tx, args).await,
        "set_bundle_state" => set_bundle_state(&tx, args).await,
        other => bail!("unknown tool '{}'", echo(other)),
    };
    match outcome {
        Ok(value) => {
            tx.commit().await.context("committing the write")?;
            Ok(value)
        }
        // Nothing half-written survives a refusal.
        Err(error) => {
            let _ = tx.rollback().await;
            Err(error)
        }
    }
}

/// The bundles in the catalog. A read, so it is served by the reader
/// connection and answers on an endpoint that holds no writer.
pub(crate) async fn list_bundles(client: &Client, args: &Value) -> Result<Value> {
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

/// Hold the lock every writer of this content bundle takes, for the rest of
/// the transaction: the web UI, another instance of this server, and this
/// one all serialize on it, so a read-modify-write cannot interleave with
/// another and drop what it wrote. The key is the bundle's **name**, which
/// is what `register_bundle_content` addresses.
async fn lock_bundle_name(tx: &Transaction<'_>, name: &str) -> Result<()> {
    tx.execute(
        "SELECT pg_advisory_xact_lock(hashtext('pgokf.content_bundle'), hashtext($1))",
        &[&name],
    )
    .await
    .context("waiting for the bundle's other writers")?;
    Ok(())
}

/// The bundle an argument names, by id or by name.
async fn resolve(tx: &Transaction<'_>, args: &Value) -> Result<Bundle> {
    let by_id = match args.get("bundle_id") {
        None | Some(Value::Null) => None,
        Some(value) => Some(
            value
                .as_i64()
                .ok_or_else(|| anyhow!("argument 'bundle_id' must be an integer"))?,
        ),
    };
    let by_name = args.get("bundle").and_then(Value::as_str);
    let row = match (by_id, by_name) {
        (None, None) => bail!("name the bundle: pass 'bundle' (its name) or 'bundle_id'"),
        (Some(_), Some(_)) => bail!("pass 'bundle' or 'bundle_id', not both"),
        (Some(id), None) => tx
            .query_opt(
                "SELECT b.id, coalesce(b.name, b.path), b.source_type, b.file_count,
                        (b.retired_at IS NOT NULL)
                 FROM pgokf.bundles b WHERE b.id = $1",
                &[&id],
            )
            .await
            .context("looking the bundle up")?
            .ok_or_else(|| anyhow!("no bundle has id {id}"))?,
        (None, Some(name)) => tx
            .query_opt(
                "SELECT b.id, coalesce(b.name, b.path), b.source_type, b.file_count,
                        (b.retired_at IS NOT NULL)
                 FROM pgokf.bundles b WHERE b.name = $1",
                &[&name],
            )
            .await
            .map_err(|error| {
                // More than one row: a name that is not this session's alone.
                anyhow!(
                    "'{}' does not name one bundle in this session: {error}. A session that is \
                     not scoped to a tenant sees every tenant's bundles, and a name may be \
                     taken in more than one; start this server with --tenant, or pass \
                     'bundle_id'",
                    echo(name)
                )
            })?
            .ok_or_else(|| {
                anyhow!(
                    "no bundle is named '{}'; list_bundles shows them",
                    echo(name)
                )
            })?,
    };
    Ok(Bundle {
        id: row.try_get(0)?,
        name: row.try_get(1)?,
        source_type: row.try_get(2)?,
        file_count: row.try_get(3)?,
        retired: row.try_get(4)?,
    })
}

/// That the bundle this write will address by name is the one that was read
/// by id, and that it is this session's to write.
///
/// `pgokf.register_bundle_content` resolves a content bundle by **name**
/// within the session's own tenant, which is not necessarily the row a
/// lookup by id found: on a session that is not scoped to a tenant, writing
/// a bundle read from one tenant would create or replace a different bundle
/// in another. Refuse rather than write to the wrong row.
async fn same_bundle(tx: &Transaction<'_>, bundle: &Bundle) -> Result<()> {
    let row = tx
        .query_one(
            "SELECT count(*), min(b.id)
             FROM pgokf.bundles b
             WHERE b.name = $1 AND b.source_type = 'content'
               AND b.tenant_id
                   = coalesce(nullif(current_setting('pgokf.tenant', true), ''), 'default')",
            &[&bundle.name],
        )
        .await
        .context("checking the bundle's name")?;
    let seen: i64 = row.try_get(0)?;
    let only: Option<i64> = row.try_get(1)?;
    if seen == 1 && only == Some(bundle.id) {
        return Ok(());
    }
    bail!(
        "this server cannot write bundle {}: a write addresses a content bundle by name, and \
         in this server's own tenant '{}' is not that bundle alone ({seen} match). A bundle \
         belonging to another tenant is written by a server started with that --tenant",
        bundle.id,
        echo(&bundle.name)
    )
}

/// That the catalog keeps document sources. Without them a bundle cannot be
/// read back to be written whole, so a write would drop every document it
/// did not carry.
async fn ensure_sources(tx: &Transaction<'_>) -> Result<()> {
    let row = tx
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

/// That nothing in this bundle would be lost by writing it back from what
/// the catalog stores.
///
/// A bundle's reserved files - `index.md`, which carries its `okf_version`,
/// and `log.md`, which carries its changelog - are read at sync time and
/// are not concepts, so the catalog keeps no bytes to write back. A full
/// snapshot that omitted them would silently drop the bundle's version and
/// its whole log, so a bundle that has them is refused instead.
async fn ensure_nothing_lost(tx: &Transaction<'_>, bundle: &Bundle) -> Result<()> {
    let row = tx
        .query_one(
            "SELECT b.okf_version IS NOT NULL,
                    EXISTS (SELECT 1 FROM pgokf.bundle_log l WHERE l.bundle_id = b.id)
             FROM pgokf.bundles b WHERE b.id = $1",
            &[&bundle.id],
        )
        .await
        .context("checking what the bundle carries")?;
    let has_index: bool = row.try_get(0)?;
    let has_log: bool = row.try_get(1)?;
    if !has_index && !has_log {
        return Ok(());
    }
    let carries = match (has_index, has_log) {
        (true, true) => "an index.md and a log.md",
        (true, false) => "an index.md",
        _ => "a log.md",
    };
    bail!(
        "bundle {} ({}) carries {carries}. Those are the bundle's own bookkeeping, not \
         documents, so the catalog keeps no bytes of them - and writing one document rewrites \
         the whole bundle from what it does keep, which would drop them. The write is refused \
         rather than made at that cost. A bundle an ingestion companion streams in is changed \
         at its source and re-synced; one built here can be re-created without those files",
        bundle.id,
        echo(&bundle.name)
    )
}

/// That this write will not put more into memory than this process is
/// willing to hold.
fn ensure_small_enough(bundle: &Bundle) -> Result<()> {
    if bundle.file_count > MAX_SNAPSHOT_FILES {
        bail!(
            "bundle {} holds {} files; writing one document rewrites the whole bundle, and this \
             server rewrites at most {MAX_SNAPSHOT_FILES}",
            bundle.id,
            bundle.file_count
        );
    }
    Ok(())
}

/// The paths a document write may not take: a skill package's manifest, and
/// anything inside a package this bundle already holds. A package is served
/// to agents whole by `get_skill`, scripts included, so it is not something
/// a contribution edits its way into.
async fn ensure_not_a_package(tx: &Transaction<'_>, bundle: &Bundle, path: &str) -> Result<()> {
    if path == "SKILL.md" || path.ends_with("/SKILL.md") {
        bail!(
            "{} would be a skill package's manifest. A package is served to agents whole, \
             scripts included, so it is not written through this API",
            echo(path)
        );
    }
    let roots = tx
        .query(
            "SELECT package_root FROM pgokf.skills WHERE bundle_id = $1",
            &[&bundle.id],
        )
        .await
        .context("reading the bundle's skill packages")?;
    for row in &roots {
        let root: String = row.try_get(0)?;
        if !root.is_empty() && path.starts_with(&format!("{root}/")) {
            bail!(
                "{} is inside the skill package {root}, which is served to agents whole; it is \
                 not written through this API",
                echo(path)
            );
        }
    }
    Ok(())
}

/// Every file of a content bundle as it stands, ready to be sent back.
async fn snapshot(tx: &Transaction<'_>, bundle_id: i64) -> Result<Vec<(String, Vec<u8>)>> {
    // Asked before the bytes are fetched: reading them to find out how many
    // there were would be the very thing the ceiling is for.
    let held: i64 = tx
        .query_one(
            "SELECT coalesce(sum(octet_length(
                 coalesce(sk.skill_md, sc.exact_bytes, rd.exact_bytes, s.raw_content)
             )), 0)::bigint
             FROM pgokf.concepts c
             LEFT JOIN pgokf.concept_source s
                    ON s.bundle_id = c.bundle_id AND s.concept_id = c.id
             LEFT JOIN pgokf.skills sk ON sk.bundle_id = c.bundle_id AND sk.concept_id = c.id
             LEFT JOIN pgokf.scripts sc ON sc.bundle_id = c.bundle_id AND sc.concept_id = c.id
             LEFT JOIN pgokf.reference_documents rd
                    ON rd.bundle_id = c.bundle_id AND rd.concept_id = c.id
             WHERE c.bundle_id = $1",
            &[&bundle_id],
        )
        .await
        .context("measuring the bundle")?
        .try_get(0)?;
    if held > MAX_SNAPSHOT_BYTES {
        bail!(
            "bundle {bundle_id} holds {} MiB of documents; writing one rewrites the whole \
             bundle, and this server does not hold more than {} MiB at once",
            held / (1024 * 1024),
            MAX_SNAPSHOT_BYTES / (1024 * 1024)
        );
    }
    let rows = tx
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
    let mut carried = 0_i64;
    for row in &rows {
        let path: String = row.try_get(0)?;
        let bytes: Option<Vec<u8>> = row.try_get(1)?;
        // A document whose bytes the catalog does not hold cannot be sent
        // back, and sending the rest would delete it. Refuse the write.
        let bytes = bytes.ok_or_else(|| {
            anyhow!(
                "the catalog holds no source for {}, so the bundle cannot be written whole \
                 without losing it; refresh the bundle with store_source on first",
                echo(&path)
            )
        })?;
        // The measure above is the bound; this is the same tally kept while
        // the rows are turned into owned buffers, in case they disagree.
        carried = carried.saturating_add(i64::try_from(bytes.len()).unwrap_or(i64::MAX));
        if carried > MAX_SNAPSHOT_BYTES {
            bail!(
                "bundle {bundle_id} holds more than {} MiB of documents; writing one rewrites \
                 the whole bundle, and this server does not hold that much at once",
                MAX_SNAPSHOT_BYTES / (1024 * 1024)
            );
        }
        files.push((path, bytes));
    }
    Ok(files)
}

/// Send a whole content bundle back, replacing what is there.
async fn resync(
    tx: &Transaction<'_>,
    bundle: &Bundle,
    files: &[(String, Vec<u8>)],
) -> Result<Value> {
    let paths: Vec<&str> = files.iter().map(|(path, _)| path.as_str()).collect();
    let contents: Vec<&[u8]> = files.iter().map(|(_, bytes)| bytes.as_slice()).collect();
    let row = tx
        .query_one(
            "SELECT to_jsonb(r) FROM pgokf.register_bundle_content($1, $2, $3, '{}'::jsonb) r",
            &[&bundle.name, &paths, &contents],
        )
        .await
        .context("writing the bundle")?;
    let outcome: Value = row.get(0);
    // The write addressed the bundle by name; prove it landed on the row
    // that was read, rather than creating or replacing another.
    let landed = outcome.get("bundle_id").and_then(Value::as_i64);
    if landed != Some(bundle.id) {
        bail!(
            "the write addressed bundle {} by name but landed on {:?}; nothing was kept",
            bundle.id,
            landed
        );
    }
    Ok(outcome)
}

/// The content bundle a document tool names, checked every way a write
/// needs before anything is read: locked, this session's, whole, and small
/// enough to rewrite.
async fn writable(tx: &Transaction<'_>, args: &Value) -> Result<Bundle> {
    ensure_sources(tx).await?;
    let bundle = resolve(tx, args).await?;
    bundle.content()?;
    lock_bundle_name(tx, &bundle.name).await?;
    // Re-read under the lock: another writer may have changed it between
    // the lookup and the lock.
    let bundle = resolve(tx, args).await?;
    bundle.content()?;
    same_bundle(tx, &bundle).await?;
    if bundle.retired {
        bail!(
            "bundle {} ({}) is retired: it is out of the catalog until it is brought back, \
             which set_bundle_state does",
            bundle.id,
            echo(&bundle.name)
        );
    }
    ensure_nothing_lost(tx, &bundle).await?;
    ensure_small_enough(&bundle)?;
    Ok(bundle)
}

async fn put_document(tx: &Transaction<'_>, args: &Value, actor: &str) -> Result<Value> {
    let path = require_path(args)?;
    let text = args
        .get("text")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("missing required string argument 'text'"))?;

    let bundle = writable(tx, args).await?;
    ensure_not_a_package(tx, &bundle, &path).await?;

    let mut files = snapshot(tx, bundle.id).await?;
    let existing = files.iter().position(|(stored, _)| *stored == path);
    // Whether this replaces a document is decided by the path being there,
    // never by whether the stored bytes happen to parse: a stored file that
    // does not parse is still a document being replaced.
    let stored = existing
        .and_then(|at| std::str::from_utf8(&files[at].1).ok())
        .and_then(|text| Document::parse(text).ok());

    let mut document = Document::parse(text).map_err(|why| anyhow!("{}: {why}", echo(&path)))?;
    let now = now_iso();
    let set_aside = if existing.is_some() {
        document.contribute_edit(actor, &now, stored.as_ref())
    } else {
        document
            .contribute_new(actor, &now)
            .map_err(|why| anyhow!("{}: {why}", echo(&path)))?
    };
    document
        .validate(&path)
        .map_err(|why| anyhow!("{}: {why}", echo(&path)))?;
    let bytes = document.render().into_bytes();

    match existing {
        Some(at) => files[at].1 = bytes,
        None => files.push((path.clone(), bytes)),
    }
    files.sort_by(|(a, _), (b, _)| a.cmp(b));
    let outcome = resync(tx, &bundle, &files).await?;

    // What the document ends up declaring, which is what the catalog derives
    // the trust tier from - not simply this token, since a contribution may
    // record the pipeline that produced it.
    let generated_by = document
        .actor_of("generated")
        .unwrap_or_else(|| actor.to_owned());
    Ok(json!({
        "bundle_id": bundle.id,
        "bundle": bundle.name,
        "path": path,
        "replaced": existing.is_some(),
        "written_by": actor,
        "generated_by": generated_by,
        "verifications_set_aside": set_aside,
        "sync": outcome,
        "note": "Written as machine-generated. A person reviews it in the catalog's web UI; \
                 until then its trust tier is not human-reviewed.",
    }))
}

async fn delete_document(tx: &Transaction<'_>, args: &Value) -> Result<Value> {
    let path = require_path(args)?;
    let bundle = writable(tx, args).await?;
    // A package is served to agents whole; taking a file out of one - its
    // manifest most of all - is not a document deletion.
    ensure_not_a_package(tx, &bundle, &path).await?;

    let mut files = snapshot(tx, bundle.id).await?;
    let before = files.len();
    files.retain(|(stored, _)| *stored != path);
    if files.len() == before {
        bail!(
            "{} holds no document at {}",
            echo(&bundle.name),
            echo(&path)
        );
    }
    let outcome = resync(tx, &bundle, &files).await?;

    Ok(json!({
        "bundle_id": bundle.id,
        "bundle": bundle.name,
        "path": path,
        "deleted": true,
        "sync": outcome,
    }))
}

async fn create_content_bundle(tx: &Transaction<'_>, args: &Value) -> Result<Value> {
    let name = args
        .get("name")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .ok_or_else(|| anyhow!("missing required string argument 'name'"))?;
    // Registering an existing content bundle with no files would empty it,
    // so the check and the creation are one: the lock is what every other
    // writer of this name takes too.
    lock_bundle_name(tx, name).await?;
    let taken: bool = tx
        .query_one(
            "SELECT EXISTS (SELECT 1 FROM pgokf.bundles WHERE name = $1 OR path = 'content:' || $1)",
            &[&name],
        )
        .await
        .context("checking the bundle name")?
        .try_get(0)?;
    if taken {
        bail!("a bundle named '{}' already exists", echo(name));
    }
    let row = tx
        .query_one(
            "SELECT to_jsonb(r)
             FROM pgokf.register_bundle_content($1, '{}'::text[], '{}'::bytea[], '{}'::jsonb) r",
            &[&name],
        )
        .await
        .context("creating the bundle")?;
    Ok(json!({ "created": name, "sync": row.get::<_, Value>(0) }))
}

async fn refresh_bundle(tx: &Transaction<'_>, args: &Value) -> Result<Value> {
    let bundle = resolve(tx, args).await?;
    if bundle.source_type == "content" {
        bail!(
            "bundle {} ({}) is a content bundle: it has no source to re-read, and its documents \
             are written with put_document",
            bundle.id,
            echo(&bundle.name)
        );
    }
    let row = tx
        .query_one(
            "SELECT to_jsonb(r) FROM pgokf.refresh_bundle($1) r",
            &[&bundle.id],
        )
        .await
        .context("refreshing the bundle")?;
    Ok(json!({ "bundle_id": bundle.id, "bundle": bundle.name, "sync": row.get::<_, Value>(0) }))
}

async fn set_bundle_state(tx: &Transaction<'_>, args: &Value) -> Result<Value> {
    let state = args
        .get("state")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("missing required string argument 'state'"))?;
    let bundle = resolve(tx, args).await?;
    let sql = match state {
        "enabled" => "SELECT pgokf.set_bundle_enabled($1, true)",
        "disabled" => "SELECT pgokf.set_bundle_enabled($1, false)",
        "retired" => "SELECT pgokf.retire_bundle($1)",
        "active" => "SELECT pgokf.unretire_bundle($1)",
        other => bail!(
            "'state' is enabled, disabled, retired, or active, not '{}'",
            echo(other)
        ),
    };
    tx.execute(sql, &[&bundle.id])
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
            assert!(
                is_write_tool(name) || name == "list_bundles",
                "{name} is declared but not dispatched"
            );
        }
        assert!(
            !is_write_tool("list_bundles"),
            "listing only reads, so it must not need the writer connection"
        );
        assert_eq!(tools.len(), 6);
    }

    #[test]
    fn a_filesystem_bundle_is_not_written_through_this_api() {
        // Arrange
        let content = Bundle {
            id: 1,
            name: "team-docs".to_owned(),
            source_type: "content".to_owned(),
            file_count: 3,
            retired: false,
        };
        let directory = Bundle {
            id: 2,
            name: "handbook".to_owned(),
            source_type: "filesystem".to_owned(),
            file_count: 3,
            retired: false,
        };

        // Act / Assert
        assert!(content.content().is_ok());
        let refused = directory.content().expect_err("refused");
        assert!(refused.to_string().contains("refresh_bundle"), "{refused}");
    }

    #[test]
    fn a_bundle_too_large_to_hold_is_refused_before_it_is_read() {
        // Arrange
        let small = Bundle {
            id: 1,
            name: "team-docs".to_owned(),
            source_type: "content".to_owned(),
            file_count: MAX_SNAPSHOT_FILES,
            retired: false,
        };
        let huge = Bundle {
            file_count: MAX_SNAPSHOT_FILES + 1,
            ..Bundle {
                id: 2,
                name: "everything".to_owned(),
                source_type: "content".to_owned(),
                file_count: 0,
                retired: false,
            }
        };

        // Act / Assert
        assert!(ensure_small_enough(&small).is_ok());
        let refused = ensure_small_enough(&huge).expect_err("refused");
        assert!(refused.to_string().contains("rewrites the whole bundle"));
    }

    #[test]
    fn an_error_repeats_only_a_bounded_piece_of_what_the_caller_sent() {
        // Arrange
        let long = "a".repeat(5_000);

        // Act
        let echoed = echo(&long);

        // Assert
        assert_eq!(
            echoed.chars().count(),
            ECHO_MAX + 1,
            "bounded, with an ellipsis"
        );
        assert!(echoed.ends_with('…'));
        assert_eq!(echo("short"), "short");
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
