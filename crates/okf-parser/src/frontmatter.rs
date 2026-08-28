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
/// `path` is the normalized bundle-relative path of the file being parsed; it
/// is attached to every error for per-file diagnostics.
///
/// The block is delimited by the first line whose entire content is `---`.
/// This line-based split intentionally avoids a YAML re-implementation, so a
/// bare `---` on its own line is always treated as the closing delimiter —
/// even when it appears inside a multiline quoted scalar. In that (rare) case
/// the YAML block is cut short and `serde_yaml` reports an unterminated
/// scalar, surfaced here as [`Error::InvalidFrontmatter`] whose message points
/// at the offending line and column. Author such values on a single line or
/// with a block scalar (`|`/`>`) to avoid the ambiguity.
///
/// # Errors
/// Returns an error for a missing delimiter, an oversized or unterminated
/// block, invalid YAML, or metadata that cannot be represented as JSON.
pub fn parse<'source>(
    source: &'source str,
    path: &str,
    max_bytes: usize,
) -> Result<(Frontmatter, &'source str)> {
    let after_open = source
        .strip_prefix("---\n")
        .or_else(|| source.strip_prefix("---\r\n"))
        .ok_or_else(|| Error::MissingFrontmatter {
            path: path.to_owned(),
        })?;

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
                path: path.to_owned(),
                actual: consumed,
                limit: max_bytes,
            });
        }
    }

    let (yaml_end, body_start) =
        yaml_end
            .zip(body_start)
            .ok_or_else(|| Error::UnterminatedFrontmatter {
                path: path.to_owned(),
            })?;
    if yaml_end > max_bytes {
        return Err(Error::FrontmatterTooLarge {
            path: path.to_owned(),
            actual: yaml_end,
            limit: max_bytes,
        });
    }

    let raw: RawFrontmatter = serde_yaml::from_str(&after_open[..yaml_end]).map_err(|source| {
        Error::InvalidFrontmatter {
            path: path.to_owned(),
            source,
        }
    })?;
    let invalid_metadata = |source| Error::InvalidMetadata {
        path: path.to_owned(),
        source,
    };
    let resource = raw
        .resource
        .map(serde_json::to_value)
        .transpose()
        .map_err(invalid_metadata)?;
    let metadata = raw
        .extra
        .into_iter()
        .map(|(key, value)| {
            serde_json::to_value(value)
                .map(|value| (key, value))
                .map_err(invalid_metadata)
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

/// The only field OKF v0.2 permits in a bundle-root `index.md` frontmatter.
#[derive(Debug, Deserialize)]
struct IndexFrontmatter {
    #[serde(default)]
    okf_version: Option<serde_yaml::Value>,
}

/// Extract the optional `okf_version` from a bundle-root `index.md`.
///
/// Per OKF v0.2, `index.md` is a reserved file (never a concept) whose
/// frontmatter may carry only an optional `okf_version`. This reader is fully
/// defensive: any failure — non-UTF-8 bytes, a missing or unterminated
/// frontmatter block, an oversized block, invalid YAML, or an absent /
/// non-scalar `okf_version` — yields `None` rather than an error, so a
/// malformed `index.md` can never abort a bundle sync. A scalar `okf_version`
/// (string or number) is returned trimmed; an empty string yields `None`.
///
/// `max_bytes` bounds the scanned frontmatter block, mirroring the concept
/// frontmatter limit, so a pathological `index.md` cannot force unbounded work.
#[must_use]
pub fn index_okf_version(source: &[u8], max_bytes: usize) -> Option<String> {
    let text = std::str::from_utf8(source).ok()?;
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let after_open = text
        .strip_prefix("---\n")
        .or_else(|| text.strip_prefix("---\r\n"))?;

    let mut consumed = 0usize;
    let mut yaml_end = None;
    for segment in after_open.split_inclusive('\n') {
        let line = segment.trim_end_matches(['\r', '\n']);
        if line == "---" {
            yaml_end = Some(consumed);
            break;
        }
        consumed += segment.len();
        if consumed > max_bytes {
            return None;
        }
    }
    let yaml_end = yaml_end?;

    let parsed: IndexFrontmatter = serde_yaml::from_str(&after_open[..yaml_end]).ok()?;
    match parsed.okf_version? {
        serde_yaml::Value::String(text) => {
            let trimmed = text.trim();
            (!trimmed.is_empty()).then(|| trimmed.to_owned())
        }
        serde_yaml::Value::Number(number) => Some(number.to_string()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::index_okf_version;

    const LIMIT: usize = 4096;

    #[test]
    fn index_okf_version_reads_a_string_version() {
        // Arrange: a reserved index.md carrying only okf_version.
        let source = b"---\nokf_version: \"0.2\"\n---\n\n# Bundle\n";

        // Act
        let version = index_okf_version(source, LIMIT);

        // Assert
        assert_eq!(version.as_deref(), Some("0.2"));
    }

    #[test]
    fn index_okf_version_reads_a_numeric_version() {
        // Arrange: YAML may parse an unquoted 0.2 as a number.
        let source = b"---\nokf_version: 0.2\n---\n";

        // Act
        let version = index_okf_version(source, LIMIT);

        // Assert
        assert_eq!(version.as_deref(), Some("0.2"));
    }

    #[test]
    fn index_okf_version_is_none_when_absent() {
        // Arrange: a frontmatter block without the key.
        let source = b"---\ntitle: Bundle index\n---\n";

        // Act / Assert
        assert_eq!(index_okf_version(source, LIMIT), None);
    }

    #[test]
    fn index_okf_version_is_none_without_frontmatter() {
        // Arrange: a plain body, no frontmatter delimiter.
        let source = b"# Bundle\n\nNo frontmatter here.\n";

        // Act / Assert
        assert_eq!(index_okf_version(source, LIMIT), None);
    }

    #[test]
    fn index_okf_version_is_none_for_invalid_yaml() {
        // Arrange: malformed YAML must degrade to None, never an error.
        let source = b"---\nokf_version: : :\n---\n";

        // Act / Assert
        assert_eq!(index_okf_version(source, LIMIT), None);
    }
}
