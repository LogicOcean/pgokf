// SPDX-License-Identifier: AGPL-3.0-only
//! What goes into a plugin: the selectors of spec §21.1 resolved through the
//! reader API, so tenant scope, visibility, retirement, and non-disclosure
//! are the database's decisions, never this crate's.

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
    #[serde(skip)]
    pub bytes: Vec<u8>,
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
                      r.ord
               FROM pgokf.concepts c
               JOIN pgokf.bundles b ON b.id = c.bundle_id AND b.enabled AND b.retired_at IS NULL
               LEFT JOIN pgokf.concept_provenance p
                      ON p.bundle_id = c.bundle_id AND p.concept_id = c.id
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
    rows.iter()
        .map(|r| {
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
            })
        })
        .collect()
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

/// Load each record's content: the exact stored source through the audited
/// `get_concept_source()` (an export is exactly what that log is for), or a
/// document reconstructed from the indexed fields when the catalog keeps
/// no source. A concept that disappeared (or was hidden) between resolving
/// and loading is dropped from the list.
///
/// # Errors
///
/// A catalog failure other than "no stored source".
pub async fn load_sources<C: GenericClient>(
    client: &C,
    records: &mut Vec<ConceptRecord>,
) -> Result<()> {
    for record in records.iter_mut() {
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
    records.retain(|r| !r.bytes.is_empty());
    Ok(())
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
        }
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
