// SPDX-License-Identifier: AGPL-3.0-only
//! Provider expectations are derived from extension identities and durable
//! configuration, independently of runtime indexes.
use crate::catalog::{Provider, RuntimeIndex, SEARCH_INDEX};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

pub const TEXT_EXPRESSION: &str = "((((COALESCE(title, ''::text) || ' '::text) || COALESCE(description, ''::text)) || ' '::text) || body_text)";
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Context {
    pub provider_schema: Option<String>,
    /// Default BM25 opclasses for the five fixed concept column types. The
    /// collector resolves these through provider extension membership, never
    /// through an existing index's indclass.
    pub search_opclasses: Vec<String>,
    pub text_config: String,
    pub vector_schema: Option<String>,
    pub embedding_dimensions: u32,
}
fn quote(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}
fn identity(schema: &str, name: &str) -> String {
    let simple = |s: &str| {
        !s.is_empty()
            && s.bytes()
                .enumerate()
                .all(|(i, b)| b.is_ascii_lowercase() || b == b'_' || (i > 0 && b.is_ascii_digit()))
    };
    format!(
        "{}.{}",
        if simple(schema) {
            schema.into()
        } else {
            quote(schema)
        },
        if simple(name) {
            name.into()
        } else {
            quote(name)
        }
    )
}
/// # Errors
/// Rejects forged runtime metadata even when shared by a reference.
pub fn validate(index: &RuntimeIndex, provider: Provider, context: &Context) -> Result<(), String> {
    if index.unique
        || index.primary
        || index.exclusion
        || !index.immediate
        || index.clustered
        || index.replica_identity
        || index.nulls_not_distinct
        || !index.included_columns.is_empty()
        || index.predicate.is_some()
        || !index.acl.is_empty()
        || index.comment.is_some()
        || !index.security_labels.is_empty()
        || index.persistence != "p"
        || index.tablespace != "pg_default"
        || index.keys.iter().any(|key| key.options != 0)
    {
        return Err(format!(
            "noncanonical runtime index metadata: {}",
            index.name
        ));
    }
    if index.name == SEARCH_INDEX {
        let schema = context
            .provider_schema
            .as_deref()
            .ok_or("provider extension schema absent")?;
        match provider {
            Provider::Plain => return Err("plain provider cannot own a BM25 index".into()),
            Provider::PgTextsearch => {
                if index.keys.len() != 1
                    || index.keys[0].expression != TEXT_EXPRESSION
                    || index.keys[0].opclass != identity(schema, "text_bm25_ops")
                    || index.keys[0].collation.as_deref() != Some("\"default\"")
                    || index.options
                        != BTreeMap::from([("text_config".into(), context.text_config.clone())])
                {
                    return Err("noncanonical pg_textsearch index shape".into());
                }
            }
            Provider::PgSearch => {
                let expressions: Vec<_> = index
                    .keys
                    .iter()
                    .map(|key| key.expression.as_str())
                    .collect();
                let opclasses: Vec<_> = index.keys.iter().map(|key| key.opclass.clone()).collect();
                let collations: Vec<_> = index
                    .keys
                    .iter()
                    .map(|key| key.collation.as_deref())
                    .collect();
                if expressions != ["id", "title", "description", "body_text", "type"]
                    || context.search_opclasses != vec![identity(schema, "anyelement_bm25_ops"); 5]
                    || opclasses != context.search_opclasses
                    || context
                        .search_opclasses
                        .iter()
                        .any(|name| !name.starts_with(&format!("{schema}.")))
                    || collations != [Some("\"default\""); 5]
                    || index.options != BTreeMap::from([("key_field".into(), "id".into())])
                {
                    return Err(format!(
                        "noncanonical pg_search index shape: {index:?}; context: {context:?}"
                    ));
                }
            }
        }
    } else {
        let schema = context
            .vector_schema
            .as_deref()
            .ok_or("vector extension absent")?;
        let dim = context.embedding_dimensions;
        if dim == 0 || dim > 2000 {
            return Err("HNSW dimension ineligible".into());
        }
        let expected = format!(
            "(embedding::{})",
            format_args!("{}({dim})", identity(schema, "vector"))
        );
        if index.keys.len() != 1
            || index.keys[0].opclass != identity(schema, "vector_cosine_ops")
            || index.keys[0].expression != expected
            || index.keys[0].collation.is_some()
            || !index.options.is_empty()
        {
            return Err("noncanonical HNSW index shape".into());
        }
    }
    Ok(())
}
