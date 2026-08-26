use std::collections::BTreeMap;

use serde::Deserialize;
use serde_json::{Map, Value};

use crate::{Error, Result};

#[derive(Debug, Deserialize)]
struct RawFrontmatter {
    id: Option<String>,
    #[serde(rename = "type")]
    concept_type: String,
    title: String,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    tags: Vec<String>,
    #[serde(default)]
    resource: Option<serde_yaml::Value>,
    #[serde(flatten)]
    extra: BTreeMap<String, serde_yaml::Value>,
}

/// Validated known frontmatter plus arbitrary JSON-compatible metadata.
#[derive(Debug, PartialEq)]
pub struct Frontmatter {
    pub id: Option<String>,
    pub concept_type: String,
    pub title: String,
    pub description: Option<String>,
    pub tags: Vec<String>,
    pub resource: Option<Value>,
    pub metadata: Map<String, Value>,
}

/// Split and deserialize a leading `---`-delimited YAML block.
///
/// # Errors
/// Returns an error for a missing delimiter, an oversized or unterminated
/// block, invalid YAML, or metadata that cannot be represented as JSON.
pub fn parse(source: &str, max_bytes: usize) -> Result<(Frontmatter, &str)> {
    let after_open = source
        .strip_prefix("---\n")
        .or_else(|| source.strip_prefix("---\r\n"))
        .ok_or(Error::MissingFrontmatter)?;

    let mut consumed = 0usize;
    let mut yaml_end = None;
    let mut body_start = None;
    for segment in after_open.split_inclusive('\n') {
        let line = segment.trim_end_matches(['\r', '\n']);
        if line == "---" {
            yaml_end = Some(consumed);
            body_start = Some(consumed + segment.len());
            break;
        }
        consumed += segment.len();
        if consumed > max_bytes {
            return Err(Error::FrontmatterTooLarge {
                actual: consumed,
                limit: max_bytes,
            });
        }
    }

    let (yaml_end, body_start) = yaml_end
        .zip(body_start)
        .ok_or(Error::UnterminatedFrontmatter)?;
    if yaml_end > max_bytes {
        return Err(Error::FrontmatterTooLarge {
            actual: yaml_end,
            limit: max_bytes,
        });
    }

    let raw: RawFrontmatter = serde_yaml::from_str(&after_open[..yaml_end])?;
    let resource = raw
        .resource
        .map(serde_json::to_value)
        .transpose()
        .map_err(Error::InvalidMetadata)?;
    let metadata = raw
        .extra
        .into_iter()
        .map(|(key, value)| {
            serde_json::to_value(value)
                .map(|value| (key, value))
                .map_err(Error::InvalidMetadata)
        })
        .collect::<Result<Map<_, _>>>()?;

    Ok((
        Frontmatter {
            id: raw.id,
            concept_type: raw.concept_type,
            title: raw.title,
            description: raw.description,
            tags: raw.tags,
            resource,
            metadata,
        },
        &after_open[body_start..],
    ))
}
