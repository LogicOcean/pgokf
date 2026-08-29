// SPDX-License-Identifier: AGPL-3.0-only
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::links::Link;

/// Fully parsed, database-neutral representation of an OKF concept.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ParsedConcept {
    /// Path-derived OKF concept ID: the normalized bundle-relative path
    /// without its `.md` suffix. Always derived from `path`; a
    /// producer-declared `id` never overrides it.
    pub id: String,
    /// Producer-declared `id` from the frontmatter, preserved verbatim for
    /// sync-time diagnostics (for example duplicate-`id` reports). Never used
    /// as a catalog key.
    pub declared_id: Option<String>,
    /// Normalized bundle-relative source path, including the `.md` suffix.
    pub path: String,
    /// Required OKF concept type.
    #[serde(rename = "type")]
    pub r#type: String,
    /// Required concept title.
    pub title: String,
    /// Optional short description.
    pub description: Option<String>,
    /// Frontmatter tags in declaration order.
    pub tags: Vec<String>,
    /// Optional resource declaration, converted to JSON.
    pub resource: Option<Value>,
    /// Markdown body rendered to compact plain text for search indexing.
    pub body_text: String,
    /// Extracted Markdown links in document order.
    pub links: Vec<Link>,
    /// Unknown frontmatter keys, retained as JSON so producer data survives
    /// round-tripping and future OKF versions.
    pub metadata: Map<String, Value>,
}
