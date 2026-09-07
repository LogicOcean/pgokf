// SPDX-License-Identifier: AGPL-3.0-only
//! What goes into a plugin: the selectors of spec §21.1 resolved through the
//! reader API, so tenant scope, visibility, retirement, and non-disclosure
//! are the database's decisions, never this crate's.

use std::collections::BTreeSet;

use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize};
use tokio_postgres::GenericClient;
use tokio_postgres::types::ToSql;

/// The largest selection one build materializes.
pub const MAX_CONCEPTS: usize = 500;
/// The default when a caller gives no limit.
pub const DEFAULT_LIMIT: usize = 100;

/// The selectors of one `include` entry: every field narrows, and an empty
/// selection (no selector at all) is refused rather than exporting a catalog.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Selection {
    #[serde(default)]
    pub bundle_ids: Vec<i64>,
    /// Concept ids, within the selected bundles (or any bundle).
    #[serde(default)]
    pub concept_ids: Vec<String>,
    /// All-of tag containment.
    #[serde(default)]
    pub tags: Vec<String>,
    /// Any-of concept types.
    #[serde(default)]
    pub types: Vec<String>,
    /// Full-text query (websearch syntax) ranked by `concept_search`.
    #[serde(default)]
    pub query: Option<String>,
    /// Only `human-reviewed` / `machine-confirmed` concepts (spec policy
    /// `trust: verified-only`).
    #[serde(default)]
    pub verified_only: bool,
    #[serde(default)]
    pub limit: Option<usize>,
}

impl Selection {
    /// `true` when at least one selector narrows the catalog.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.bundle_ids.is_empty()
            && self.concept_ids.is_empty()
            && self.tags.is_empty()
            && self.types.is_empty()
            && self.query.as_deref().is_none_or(|q| q.trim().is_empty())
    }

    /// The effective row limit, bounded to [`MAX_CONCEPTS`].
    #[must_use]
    pub fn effective_limit(&self) -> usize {
        self.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_CONCEPTS)
    }

    /// A short human description ("type Runbook, tagged a, b, in bundle 2").
    #[must_use]
    pub fn describe(&self) -> String {
        let mut parts = Vec::new();
        if let Some(q) = self.query.as_deref().filter(|q| !q.trim().is_empty()) {
            parts.push(format!("matching \u{201c}{}\u{201d}", q.trim()));
        }
        if !self.types.is_empty() {
            parts.push(format!("of type {}", self.types.join(" or ")));
        }
        if !self.tags.is_empty() {
            parts.push(format!("tagged {}", self.tags.join(", ")));
        }
        if !self.concept_ids.is_empty() {
            parts.push(format!("{} named concept(s)", self.concept_ids.len()));
        }
        if !self.bundle_ids.is_empty() {
            let ids: Vec<String> = self.bundle_ids.iter().map(ToString::to_string).collect();
            parts.push(format!("in bundle(s) {}", ids.join(", ")));
        }
        if self.verified_only {
            parts.push("verified only".to_owned());
        }
        if parts.is_empty() {
            "nothing selected".to_owned()
        } else {
            parts.join(", ")
        }
    }
}

/// One concept as the builder materializes it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ConceptRecord {
    pub bundle_id: i64,
    pub bundle_name: String,
    pub concept_id: String,
    pub path: String,
    pub title: Option<String>,
    pub description: Option<String>,
    pub concept_type: Option<String>,
    pub tags: Vec<String>,
    pub file_hash: String,
    pub trust_tier: String,
    pub status: String,
    /// `true` when `bytes` are the exact stored source; `false` when the
    /// catalog keeps no source and the document was reconstructed from the
    /// indexed text.
    pub exact: bool,
    /// The document as written to the workspace (empty until sources load).
    /// For a skill this is the exact `SKILL.md`; for a package resource its
    /// exact bytes.
    #[serde(skip)]
    pub bytes: Vec<u8>,
    /// Present when the concept is an Agent Skills package manifest: the
    /// package identity from `pgokf.skills`, and its resources with their
    /// bytes once sources load. The whole package is materialized under one
    /// directory, byte for byte.
    pub package: Option<PackageRecord>,
    /// Present when the concept is a package resource (a script, reference,
    /// or asset) selected on its own; dropped when its package is selected
    /// too, because the package already carries it.
    pub resource: Option<ResourceRecord>,
}

/// A skill package: what `pgokf.skills` records plus, after loading, every
/// resource the package owns.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PackageRecord {
    /// The Agent Skills `name` (the directory the harness expects).
    pub name: String,
    /// Bundle-relative package directory (`""` for a root package).
    pub root: String,
    /// The catalog's package hash over the manifest and every member.
    pub hash: String,
    /// The package's scripts, references, and assets, in path order.
    pub resources: Vec<ResourceFile>,
}

/// One file of a skill package.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ResourceFile {
    /// The resource's own concept id (its bundle-relative path).
    pub concept_id: String,
    /// `script`, `reference`, or `asset`.
    pub class: String,
    /// Package-relative path (`scripts/check.sh`).
    pub path: String,
    /// SHA-256 of the exact bytes, as the catalog records it.
    pub sha256: String,
    /// The catalog's BLAKE3 `file_hash` of the resource's own concept.
    pub file_hash: String,
    /// The exact bytes (empty until sources load).
    #[serde(skip)]
    pub bytes: Vec<u8>,
}

impl ResourceFile {
    /// Scripts keep their executable bit in the workspace.
    #[must_use]
    pub fn is_script(&self) -> bool {
        self.class == "script"
    }
}

/// A package resource selected as a concept of its own.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ResourceRecord {
    /// `script`, `reference`, or `asset`.
    pub class: String,
    /// Package-relative path.
    pub source_path: String,
    /// The owning skill's concept id.
    pub package_concept_id: String,
    /// SHA-256 of the exact bytes, as the catalog records it.
    pub sha256: String,
}

/// The catalog identity a lockfile records: versions and the state of each
/// included bundle, so an unchanged catalog reproduces the same tree.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Snapshot {
    pub version: String,
    pub sql_version: String,
    pub bundles: Vec<BundleState>,
}

/// One bundle's identity in the snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BundleState {
    pub id: i64,
    pub name: String,
    pub sync_hash: Option<String>,
    pub last_synced_at: Option<String>,
}

const TRUSTED_TIERS: [&str; 2] = ["human-reviewed", "machine-confirmed"];

/// Resolve a selection to concept records without their content (a preview,
/// or the first step of a build). Ordered by bundle then concept id; a query
/// selection is ordered by rank instead.
///
/// # Errors
///
/// An empty selection, or a catalog query failure.
pub async fn resolve<C: GenericClient>(
    client: &C,
    selection: &Selection,
) -> Result<Vec<ConceptRecord>> {
    if selection.is_empty() {
        return Err(anyhow!(
            "the selection is empty: give a query, a bundle, a type, a tag, or concept ids"
        ));
    }
    let limit = i64::try_from(selection.effective_limit()).unwrap_or(i64::MAX);
    let bundle_ids = (!selection.bundle_ids.is_empty()).then(|| selection.bundle_ids.clone());
    let types = (!selection.types.is_empty()).then(|| selection.types.clone());
    let tags = (!selection.tags.is_empty()).then(|| selection.tags.clone());
    let concept_ids = (!selection.concept_ids.is_empty()).then(|| selection.concept_ids.clone());
    let trusted = selection
        .verified_only
        .then(|| TRUSTED_TIERS.map(str::to_owned).to_vec());

    let (ranked_bundles, ranked_ids) = match ranked_ids(client, selection).await? {
        Some((b, c)) => (Some(b), Some(c)),
        None => (None, None),
    };

    let sql = "SELECT c.bundle_id, coalesce(b.name, regexp_replace(b.path, '^.*/', '')),
                      c.id, c.path, c.title, c.description, c.type, coalesce(c.tags, '{}'),
                      c.file_hash, coalesce(p.trust_tier, 'unverified'), coalesce(p.status, 'stable'),
                      r.ord,
                      sk.agent_skill->>'name', sk.package_root, sk.package_hash,
                      CASE WHEN s.concept_id IS NOT NULL THEN 'script'
                           WHEN d.source_path LIKE 'assets/%' THEN 'asset'
                           WHEN d.concept_id IS NOT NULL THEN 'reference' END,
                      coalesce(s.source_path, d.source_path),
                      coalesce(s.package_concept_id, d.package_concept_id),
                      coalesce(s.executable_sha256, d.content_sha256)
               FROM pgokf.concepts c
               JOIN pgokf.bundles b ON b.id = c.bundle_id AND b.enabled AND b.retired_at IS NULL
               LEFT JOIN pgokf.concept_provenance p
                      ON p.bundle_id = c.bundle_id AND p.concept_id = c.id
               LEFT JOIN pgokf.skills sk ON sk.bundle_id = c.bundle_id AND sk.concept_id = c.id
               LEFT JOIN pgokf.scripts s ON s.bundle_id = c.bundle_id AND s.concept_id = c.id
               LEFT JOIN pgokf.reference_documents d
                      ON d.bundle_id = c.bundle_id AND d.concept_id = c.id
               LEFT JOIN (
                   SELECT o.b, o.id, min(o.ord) AS ord
                   FROM ROWS FROM (unnest($6::bigint[]), unnest($7::text[])) WITH ORDINALITY AS o(b, id, ord)
                   GROUP BY o.b, o.id
               ) r ON r.b = c.bundle_id AND r.id = c.id
               WHERE ($1::bigint[] IS NULL OR c.bundle_id = ANY($1))
                 AND ($2::text[] IS NULL OR c.type = ANY($2))
                 AND ($3::text[] IS NULL OR c.tags @> $3)
                 AND ($4::text[] IS NULL OR c.id = ANY($4))
                 AND ($5::text[] IS NULL OR coalesce(p.trust_tier, 'unverified') = ANY($5))
                 AND ($6::bigint[] IS NULL OR r.ord IS NOT NULL)
               ORDER BY r.ord NULLS LAST, c.bundle_id, c.id
               LIMIT $8";
    let params: [&(dyn ToSql + Sync); 8] = [
        &bundle_ids,
        &types,
        &tags,
        &concept_ids,
        &trusted,
        &ranked_bundles,
        &ranked_ids,
        &limit,
    ];
    let rows = client
        .query(sql, &params)
        .await
        .context("resolving the selection")?;
    rows.iter().map(record_from_row).collect()
}

/// One resolved row as a [`ConceptRecord`] (without content).
fn record_from_row(r: &tokio_postgres::Row) -> Result<ConceptRecord> {
    Ok(ConceptRecord {
        bundle_id: r.try_get(0)?,
        bundle_name: r.try_get(1)?,
        concept_id: r.try_get(2)?,
        path: r.try_get(3)?,
        title: r.try_get(4)?,
        description: r.try_get(5)?,
        concept_type: r.try_get(6)?,
        tags: r.try_get(7)?,
        file_hash: r.try_get(8)?,
        trust_tier: r.try_get(9)?,
        status: r.try_get(10)?,
        exact: false,
        bytes: Vec::new(),
        package: package_from_row(r)?,
        resource: resource_from_row(r)?,
    })
}

/// The `pgokf.skills` columns of a resolved row, when the concept is a
/// manifest.
fn package_from_row(r: &tokio_postgres::Row) -> Result<Option<PackageRecord>> {
    let name: Option<String> = r.try_get(12)?;
    let root: Option<String> = r.try_get(13)?;
    let hash: Option<String> = r.try_get(14)?;
    Ok(match (root, hash) {
        (Some(root), Some(hash)) => Some(PackageRecord {
            // A manifest always has a name; fall back to the directory only
            // if the frontmatter is somehow unnamed.
            name: name.unwrap_or_else(|| root.rsplit('/').next().unwrap_or("skill").to_owned()),
            root,
            hash,
            resources: Vec::new(),
        }),
        _ => None,
    })
}

/// The resource columns of a resolved row, when the concept is a package
/// script, reference, or asset.
fn resource_from_row(r: &tokio_postgres::Row) -> Result<Option<ResourceRecord>> {
    let class: Option<String> = r.try_get(15)?;
    let source_path: Option<String> = r.try_get(16)?;
    let package_concept_id: Option<String> = r.try_get(17)?;
    let sha256: Option<String> = r.try_get(18)?;
    Ok(match (class, source_path, package_concept_id, sha256) {
        (Some(class), Some(source_path), Some(package_concept_id), Some(sha256)) => {
            Some(ResourceRecord {
                class,
                source_path,
                package_concept_id,
                sha256,
            })
        }
        _ => None,
    })
}

/// For a query selection, the `(bundle_id, concept_id)` pairs
/// `concept_search` ranks, in rank order, restricted to the selected
/// bundles; `None` when the selection has no query text.
async fn ranked_ids<C: GenericClient>(
    client: &C,
    selection: &Selection,
) -> Result<Option<(Vec<i64>, Vec<String>)>> {
    let Some(q) = selection
        .query
        .as_deref()
        .map(str::trim)
        .filter(|q| !q.is_empty())
    else {
        return Ok(None);
    };
    let one_bundle = match selection.bundle_ids.as_slice() {
        [only] => Some(*only),
        _ => None,
    };
    let first_type = match selection.types.as_slice() {
        [only] => Some(only.clone()),
        _ => None,
    };
    let tags = (!selection.tags.is_empty()).then(|| selection.tags.clone());
    // The other selectors (several types, verified-only, ids, bundles) are
    // applied after ranking, so rank the full window and let the outer
    // query's LIMIT cut it; otherwise a page could come back short.
    let search_limit = i32::try_from(MAX_CONCEPTS).unwrap_or(i32::MAX);
    let rows = client
        .query(
            "SELECT bundle_id, concept_id
             FROM pgokf.concept_search($1, $2, $3, $4, $5, NULL, NULL, NULL)",
            &[&q, &one_bundle, &search_limit, &first_type, &tags],
        )
        .await
        .context("ranking the selection")?;
    let mut ids: (Vec<i64>, Vec<String>) = (Vec::new(), Vec::new());
    for row in rows {
        let b: i64 = row.try_get(0)?;
        let c: String = row.try_get(1)?;
        if selection.bundle_ids.is_empty() || selection.bundle_ids.contains(&b) {
            ids.0.push(b);
            ids.1.push(c);
        }
    }
    Ok(Some(ids))
}

/// Load each record's content, always through the audited readers (an
/// export is exactly what that log is for): a skill package through
/// `get_skill()` (the exact `SKILL.md`) plus `get_script()` /
/// `get_reference()` for each resource it owns; a resource selected on its
/// own through the same two; any other concept through
/// `get_concept_source()`, or a document reconstructed from the indexed
/// fields when the catalog keeps no source. A concept that disappeared (or
/// was hidden) between resolving and loading is dropped from the list, and
/// so is a resource whose package is in the selection, since the package
/// carries it.
///
/// # Errors
///
/// A catalog failure other than "no stored source".
pub async fn load_sources<C: GenericClient>(
    client: &C,
    records: &mut Vec<ConceptRecord>,
) -> Result<()> {
    drop_packaged_resources(records);
    let mut vanished: Vec<bool> = vec![false; records.len()];
    for (index, record) in records.iter_mut().enumerate() {
        if record.package.is_some() {
            vanished[index] = !load_package(client, record).await?;
            continue;
        }
        if let Some(resource) = record.resource.clone() {
            match resource_bytes(
                client,
                record.bundle_id,
                &record.concept_id,
                &resource.class,
            )
            .await?
            {
                Some(bytes) => {
                    verify_sha256(&bytes, &resource.sha256, &record.concept_id)?;
                    record.bytes = bytes;
                    record.exact = true;
                }
                None => vanished[index] = true,
            }
            continue;
        }
        let source = client
            .query_opt(
                "SELECT pgokf.get_concept_source($1, $2)",
                &[&record.bundle_id, &record.concept_id],
            )
            .await;
        match source {
            Ok(Some(row)) => {
                record.bytes = row.try_get::<_, Option<Vec<u8>>>(0)?.unwrap_or_default();
                record.exact = !record.bytes.is_empty();
            }
            // invalid_parameter_value: no source stored for this concept.
            Err(error)
                if error
                    .as_db_error()
                    .is_some_and(|e| e.code().code() == "22023") =>
            {
                record.exact = false;
            }
            Ok(None) => record.exact = false,
            Err(error) => return Err(error).context("reading a concept's stored source"),
        }
        if !record.exact {
            let body: Option<String> = client
                .query_opt(
                    "SELECT body_text FROM pgokf.concepts WHERE bundle_id = $1 AND id = $2",
                    &[&record.bundle_id, &record.concept_id],
                )
                .await
                .context("reading a concept's indexed text")?
                .map(|row| row.try_get(0))
                .transpose()?;
            record.bytes = body
                .map(|b| reconstruct(record, &b).into_bytes())
                .unwrap_or_default();
        }
    }
    let mut index = 0;
    records.retain(|r| {
        // A document that loaded nothing vanished; a resource may legitimately
        // be empty, so only the explicit flag drops it.
        let keep = !vanished[index] && (r.resource.is_some() || !r.bytes.is_empty());
        index += 1;
        keep
    });
    Ok(())
}

/// Drop every resource whose package is in the selection: the package
/// carries it, byte for byte, so writing it again would duplicate the file.
/// Applied to previews and builds alike so both show the same tree.
pub fn drop_packaged_resources(records: &mut Vec<ConceptRecord>) {
    let owned: BTreeSet<(i64, String)> = records
        .iter()
        .filter(|r| r.package.is_some())
        .map(|r| (r.bundle_id, r.concept_id.clone()))
        .collect();
    records.retain(|r| {
        r.resource
            .as_ref()
            .is_none_or(|res| !owned.contains(&(r.bundle_id, res.package_concept_id.clone())))
    });
}

/// The bytes a reader returned must hash to what the listing promised;
/// otherwise the catalog changed between the two reads and the tree would
/// mix versions.
fn verify_sha256(bytes: &[u8], expected: &str, concept_id: &str) -> Result<()> {
    let actual = sha256_hex(bytes);
    if !expected.is_empty() && actual != expected {
        return Err(anyhow!(
            "resource {concept_id} changed while the plugin was being built (expected \
             sha256 {expected}, read {actual}); build again"
        ));
    }
    Ok(())
}

/// Lowercase hex SHA-256 of a buffer.
#[must_use]
pub fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    use std::fmt::Write as _;
    Sha256::digest(bytes)
        .iter()
        .fold(String::with_capacity(64), |mut out, b| {
            let _ = write!(out, "{b:02x}");
            out
        })
}

/// Fill a skill record with its exact manifest and every resource's bytes.
/// Returns `false` when the package vanished (or was hidden) between
/// resolving and loading, so the caller drops the record. A listed member
/// that cannot be read, or whose bytes no longer hash as listed, is an error:
/// the catalog changed under the build and the tree would mix versions.
async fn load_package<C: GenericClient>(client: &C, record: &mut ConceptRecord) -> Result<bool> {
    let row = match client
        .query_opt(
            "SELECT s.skill_md, s.package_root, s.package_hash, s.agent_skill->>'name', s.resources
             FROM pgokf.get_skill($1, $2) AS s",
            &[&record.bundle_id, &record.concept_id],
        )
        .await
    {
        Ok(row) => row,
        Err(error) if is_invalid_parameter(&error) => None,
        Err(error) => return Err(error).context("reading a skill package"),
    };
    let Some(row) = row else {
        record.bytes.clear();
        return Ok(false);
    };
    let skill_md: Vec<u8> = row.try_get(0)?;
    let root: String = row.try_get(1)?;
    let hash: String = row.try_get(2)?;
    let name: Option<String> = row.try_get(3)?;
    let listing: serde_json::Value = row.try_get(4)?;
    let mut resources = Vec::new();
    for entry in listing.as_array().into_iter().flatten() {
        let concept_id = entry["concept_id"].as_str().unwrap_or_default().to_owned();
        let class = entry["class"].as_str().unwrap_or("reference").to_owned();
        let path = entry["path"].as_str().unwrap_or_default().to_owned();
        let sha256 = entry["sha256"].as_str().unwrap_or_default().to_owned();
        let file_hash = entry["file_hash"].as_str().unwrap_or_default().to_owned();
        if concept_id.is_empty() || path.is_empty() {
            continue;
        }
        let Some(bytes) = resource_bytes(client, record.bundle_id, &concept_id, &class).await?
        else {
            return Err(anyhow!(
                "package {} lists {concept_id} but it could not be read (the catalog changed \
                 while the plugin was being built); build again",
                record.concept_id
            ));
        };
        verify_sha256(&bytes, &sha256, &concept_id)?;
        resources.push(ResourceFile {
            concept_id,
            class,
            path,
            sha256,
            file_hash,
            bytes,
        });
    }
    let package = record.package.get_or_insert_with(|| PackageRecord {
        name: String::new(),
        root: String::new(),
        hash: String::new(),
        resources: Vec::new(),
    });
    if let Some(name) = name {
        package.name = name;
    }
    package.root = root;
    package.hash = hash;
    package.resources = resources;
    record.bytes = skill_md;
    record.exact = true;
    Ok(true)
}

/// The exact bytes of one package resource through the audited reader for
/// its class; `None` when it vanished or is hidden.
async fn resource_bytes<C: GenericClient>(
    client: &C,
    bundle_id: i64,
    concept_id: &str,
    class: &str,
) -> Result<Option<Vec<u8>>> {
    let sql = if class == "script" {
        "SELECT (pgokf.get_script($1, $2)).exact_bytes"
    } else {
        "SELECT (pgokf.get_reference($1, $2)).exact_bytes"
    };
    match client.query_opt(sql, &[&bundle_id, &concept_id]).await {
        Ok(Some(row)) => Ok(row.try_get::<_, Option<Vec<u8>>>(0)?),
        Ok(None) => Ok(None),
        Err(error) if is_invalid_parameter(&error) => Ok(None),
        Err(error) => Err(error).context("reading a package resource"),
    }
}

/// SQLSTATE 22023: the catalog's "no such (visible) concept".
fn is_invalid_parameter(error: &tokio_postgres::Error) -> bool {
    error
        .as_db_error()
        .is_some_and(|e| e.code().code() == "22023")
}

/// A document for a concept whose source the catalog does not keep: the
/// modeled frontmatter fields and the indexed text, marked as such.
fn reconstruct(record: &ConceptRecord, body: &str) -> String {
    use std::fmt::Write as _;
    let mut out = String::from("---\n");
    if let Some(t) = &record.concept_type {
        let _ = writeln!(out, "type: {}", yaml_string(t));
    }
    if let Some(t) = &record.title {
        let _ = writeln!(out, "title: {}", yaml_string(t));
    }
    if let Some(d) = &record.description {
        let _ = writeln!(out, "description: {}", yaml_string(d));
    }
    if !record.tags.is_empty() {
        out.push_str("tags:\n");
        for tag in &record.tags {
            let _ = writeln!(out, "  - {}", yaml_string(tag));
        }
    }
    out.push_str("---\n\n");
    out.push_str("<!-- Reconstructed from the catalog's indexed text: the bundle was ingested without store_source, so this is not the original file. -->\n\n");
    out.push_str(body);
    if !body.ends_with('\n') {
        out.push('\n');
    }
    out
}

/// A YAML scalar in the JSON-compatible double-quoted form, which every
/// YAML parser accepts and which needs no escaping rules of its own.
#[must_use]
pub fn yaml_string(value: &str) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| "\"\"".to_owned())
}

/// The catalog snapshot the lockfile records.
///
/// # Errors
///
/// A catalog query failure.
pub async fn snapshot<C: GenericClient>(client: &C, bundle_ids: &[i64]) -> Result<Snapshot> {
    let versions = client
        .query_one(
            "SELECT pgokf.version(),
                    coalesce((SELECT extversion FROM pg_catalog.pg_extension WHERE extname = 'pgokf'), '')",
            &[],
        )
        .await
        .context("reading the catalog version")?;
    let ids: Vec<i64> = bundle_ids.to_vec();
    let rows = client
        .query(
            "SELECT b.id, coalesce(b.name, regexp_replace(b.path, '^.*/', '')), b.sync_hash,
                    to_char(b.last_synced_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"')
             FROM pgokf.bundles b WHERE b.id = ANY($1) ORDER BY b.id",
            &[&ids],
        )
        .await
        .context("reading the bundle states")?;
    let bundles = rows
        .iter()
        .map(|r| {
            Ok(BundleState {
                id: r.try_get(0)?,
                name: r.try_get(1)?,
                sync_hash: r.try_get(2)?,
                last_synced_at: r.try_get(3)?,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(Snapshot {
        version: versions.try_get(0)?,
        sql_version: versions.try_get(1)?,
        bundles,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record() -> ConceptRecord {
        ConceptRecord {
            bundle_id: 1,
            bundle_name: "sample".to_owned(),
            concept_id: "runbooks/a".to_owned(),
            path: "runbooks/a.md".to_owned(),
            title: Some("A \"quoted\" title".to_owned()),
            description: None,
            concept_type: Some("Runbook".to_owned()),
            tags: vec!["x".to_owned()],
            file_hash: "abc".to_owned(),
            trust_tier: "unverified".to_owned(),
            status: "stable".to_owned(),
            exact: false,
            bytes: Vec::new(),
            package: None,
            resource: None,
        }
    }

    #[test]
    fn drop_packaged_resources_keeps_only_resources_of_absent_packages() {
        // Arrange: a package, one of its scripts, and a script of a package
        // that is not selected (and one in another bundle with the same id).
        let mut package = record();
        package.concept_id = "skills/deploy/SKILL".to_owned();
        package.package = Some(PackageRecord {
            name: "deploy".to_owned(),
            root: "skills/deploy".to_owned(),
            hash: "p".repeat(64),
            resources: Vec::new(),
        });
        let mut owned = record();
        owned.concept_id = "skills/deploy/scripts/a.sh".to_owned();
        owned.resource = Some(ResourceRecord {
            class: "script".to_owned(),
            source_path: "scripts/a.sh".to_owned(),
            package_concept_id: "skills/deploy/SKILL".to_owned(),
            sha256: String::new(),
        });
        let mut other_bundle = owned.clone();
        other_bundle.bundle_id = 2;
        let mut orphan = owned.clone();
        orphan.concept_id = "skills/other/scripts/b.sh".to_owned();
        orphan.resource.as_mut().unwrap().package_concept_id = "skills/other/SKILL".to_owned();
        let mut records = vec![package, owned, other_bundle, orphan];

        // Act
        drop_packaged_resources(&mut records);

        // Assert
        let ids: Vec<(i64, &str)> = records
            .iter()
            .map(|r| (r.bundle_id, r.concept_id.as_str()))
            .collect();
        assert_eq!(
            ids,
            [
                (1, "skills/deploy/SKILL"),
                (2, "skills/deploy/scripts/a.sh"),
                (1, "skills/other/scripts/b.sh")
            ]
        );
    }

    #[test]
    fn verify_sha256_accepts_a_match_or_an_unknown_digest_and_refuses_drift() {
        // Arrange
        let bytes = b"echo ok\n";
        let digest = sha256_hex(bytes);

        // Act / Assert
        assert!(verify_sha256(bytes, &digest, "x").is_ok());
        assert!(verify_sha256(bytes, "", "x").is_ok());
        let error = verify_sha256(bytes, &"0".repeat(64), "skills/deploy/scripts/a.sh")
            .expect_err("a different digest is refused");
        assert!(
            error
                .to_string()
                .contains("changed while the plugin was being built")
        );
    }

    #[test]
    fn an_empty_selection_is_recognised_and_described() {
        // Arrange
        let empty = Selection::default();
        let tagged = Selection {
            tags: vec!["ops".to_owned()],
            bundle_ids: vec![2],
            verified_only: true,
            ..Selection::default()
        };

        // Act & Assert
        assert!(empty.is_empty());
        assert_eq!(empty.describe(), "nothing selected");
        assert!(!tagged.is_empty());
        assert_eq!(
            tagged.describe(),
            "tagged ops, in bundle(s) 2, verified only"
        );
        assert_eq!(
            Selection {
                limit: Some(9_999),
                ..Selection::default()
            }
            .effective_limit(),
            MAX_CONCEPTS
        );
    }

    #[test]
    fn reconstruct_writes_valid_frontmatter_and_marks_the_document() {
        // Arrange
        let record = record();

        // Act
        let text = reconstruct(&record, "Body text");

        // Assert
        assert!(text.starts_with(
            "---\ntype: \"Runbook\"\ntitle: \"A \\\"quoted\\\" title\"\ntags:\n  - \"x\"\n---\n\n"
        ));
        assert!(text.contains("Reconstructed from the catalog's indexed text"));
        assert!(text.ends_with("Body text\n"));
    }
}
