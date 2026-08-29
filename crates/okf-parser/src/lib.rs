// SPDX-License-Identifier: AGPL-3.0-only
//! PostgreSQL-independent parser for Open Knowledge Format Markdown concepts.

pub mod error;
pub mod frontmatter;
pub mod limits;
pub mod links;
pub mod markdown;
pub mod model;
pub mod normalize;
pub mod version;

pub use error::{Error, ErrorCategory, Result};
pub use frontmatter::index_okf_version;
pub use limits::ParserLimits;
pub use links::{Link, LinkKind, ResolvedReference, resolve_reference};
pub use model::ParsedConcept;
pub use normalize::{
    concept_id, is_reserved_log, is_reserved_path, normalize_path, parent_directory,
    resolve_link_target,
};
pub use version::{SUPPORTED_OKF_VERSIONS, is_supported_okf_version};
// The parsed frontmatter's producer metadata is already exposed as
// `ParsedConcept::metadata: serde_json::Map<String, Value>`, so these JSON
// types are part of this crate's public surface. Re-exporting them lets
// downstream catalog projections read that metadata without taking their own
// direct `serde_json` dependency.
pub use serde_json::{Map, Value};

use std::path::Path;
use stratify::logging::tracing;

/// Parse one UTF-8 Markdown concept using the supplied bundle-relative path.
///
/// The concept ID is always derived from the normalized path (without its
/// `.md` suffix); a producer-declared frontmatter `id` is preserved as
/// [`ParsedConcept::declared_id`] for diagnostics but never trusted as the
/// catalog key. Reserved OKF files (`index.md`/`log.md`) are rejected because
/// they are not ordinary concepts; callers can skip them up front with
/// [`is_reserved_path`].
///
/// # Errors
/// Returns an error when limits are exceeded, the path is unsafe or reserved,
/// the input is not UTF-8, or the frontmatter is missing or invalid. Every
/// error carries the offending file path and a diagnostic category.
pub fn parse_concept(
    source: &[u8],
    relative_path: impl AsRef<Path>,
    limits: ParserLimits,
) -> Result<ParsedConcept> {
    let path = normalize_path(relative_path.as_ref())?;
    if is_reserved_path(&path) {
        return Err(Error::ReservedPath { path });
    }
    if source.len() > limits.max_file_bytes {
        return Err(Error::FileTooLarge {
            path,
            actual: source.len(),
            limit: limits.max_file_bytes,
        });
    }

    let source = std::str::from_utf8(source).map_err(|source| Error::InvalidUtf8 {
        path: path.clone(),
        source,
    })?;
    // A single leading UTF-8 BOM (U+FEFF) is a legal encoding artifact many
    // editors prepend; strip it so it cannot mask the `---` frontmatter
    // delimiter. Only the very first BOM is removed - any later U+FEFF is
    // genuine content and left untouched.
    let source = source.strip_prefix('\u{feff}').unwrap_or(source);
    let (frontmatter, body) = frontmatter::parse(source, &path, limits.max_frontmatter_bytes)?;
    let links = links::extract(body, &path);
    let body_text = markdown::plain_text(body);
    let id = concept_id(&path);

    tracing::debug!(path = %path, link_count = links.len(), "parsed OKF concept");

    Ok(ParsedConcept {
        id,
        declared_id: frontmatter.id,
        path,
        r#type: frontmatter.concept_type,
        title: frontmatter.title,
        description: frontmatter.description,
        tags: frontmatter.tags,
        resource: frontmatter.resource,
        body_text,
        links,
        metadata: frontmatter.metadata,
    })
}
