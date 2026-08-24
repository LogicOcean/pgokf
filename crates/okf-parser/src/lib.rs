//! PostgreSQL-independent parser for Open Knowledge Format Markdown concepts.

pub mod error;
pub mod frontmatter;
pub mod limits;
pub mod links;
pub mod markdown;
pub mod model;
pub mod normalize;

pub use error::{Error, Result};
pub use limits::ParserLimits;
pub use links::{Link, LinkKind};
pub use model::ParsedConcept;
pub use normalize::normalize_path;

use std::path::Path;
use stratify::logging::tracing;

/// Parse one UTF-8 Markdown concept using the supplied source-relative path.
///
/// # Errors
/// Returns an error when limits are exceeded, the path is unsafe, the input is
/// not UTF-8, or the frontmatter is missing or invalid.
pub fn parse_concept(
    source: &[u8],
    relative_path: impl AsRef<Path>,
    limits: ParserLimits,
) -> Result<ParsedConcept> {
    if source.len() > limits.max_file_bytes {
        return Err(Error::FileTooLarge {
            actual: source.len(),
            limit: limits.max_file_bytes,
        });
    }

    let source = std::str::from_utf8(source)?;
    let path = normalize_path(relative_path.as_ref())?;
    let (frontmatter, body) = frontmatter::parse(source, limits.max_frontmatter_bytes)?;
    let links = links::extract(body);
    let body_text = markdown::plain_text(body);
    let id = frontmatter.id.unwrap_or_else(|| path.clone());

    tracing::debug!(path = %path, link_count = links.len(), "parsed OKF concept");

    Ok(ParsedConcept {
        id,
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
