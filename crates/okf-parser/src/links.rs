use pulldown_cmark::{CowStr, Event, LinkType, Options, Parser, Tag, TagEnd};
use serde::{Deserialize, Serialize};

use crate::normalize;

/// Classification of a Markdown destination.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LinkKind {
    Inline,
    Reference,
    Autolink,
    Email,
    Image,
}

/// One link in source order.
///
/// The raw destination is always retained in `target`. For internal
/// destinations the parser additionally normalizes the target against the
/// source document's directory (see [`normalize::resolve_link_target`]);
/// resolution against actual bundle contents is the sync layer's job.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Link {
    /// Raw Markdown destination exactly as written.
    pub target: String,
    /// Plain-text label of the link.
    pub label: String,
    /// Markdown construct that produced the link.
    pub kind: LinkKind,
    /// Zero-based position of the link in document order.
    pub ordinal: usize,
    /// Whether the destination is an external URL (scheme-qualified such as
    /// `https:`/`mailto:`, or protocol-relative `//`). External destinations
    /// never become internal graph edges.
    pub is_external: bool,
    /// Normalized bundle-relative target path (with `.md`) for internal
    /// destinations; `None` for external or unresolvable destinations.
    pub target_path: Option<String>,
    /// OKF concept ID of the target (`target_path` without `.md`).
    pub target_id: Option<String>,
}

struct PendingLink {
    target: String,
    label: String,
    kind: LinkKind,
    ordinal: usize,
}

impl PendingLink {
    /// Finalize the pending link, normalizing internal destinations against
    /// the source document's path.
    fn into_link(self, source_path: &str) -> Link {
        // Email autolinks carry a bare address (no `mailto:` scheme) as their
        // destination, yet they are always external.
        let is_external = self.kind == LinkKind::Email || is_external_target(&self.target);
        let target_path = if is_external {
            None
        } else {
            normalize::resolve_link_target(&self.target, source_path)
        };
        let target_id = target_path.as_deref().map(normalize::concept_id);
        Link {
            target: self.target,
            label: self.label.trim().to_owned(),
            kind: self.kind,
            ordinal: self.ordinal,
            is_external,
            target_path,
            target_id,
        }
    }
}

/// Extract Markdown links and images while preserving document order.
///
/// `source_path` is the normalized bundle-relative path of the document being
/// parsed; internal destinations are normalized relative to its directory.
#[must_use]
pub fn extract(markdown: &str, source_path: &str) -> Vec<Link> {
    let mut links = Vec::new();
    let mut pending = Vec::<PendingLink>::new();
    let mut next_ordinal = 0;

    for event in Parser::new_ext(markdown, Options::all()) {
        match event {
            Event::Start(Tag::Link {
                link_type,
                dest_url,
                ..
            }) => {
                pending.push(PendingLink {
                    target: dest_url.into_string(),
                    label: String::new(),
                    kind: classify(link_type),
                    ordinal: next_ordinal,
                });
                next_ordinal += 1;
            }
            Event::Start(Tag::Image { dest_url, .. }) => {
                pending.push(PendingLink {
                    target: dest_url.into_string(),
                    label: String::new(),
                    kind: LinkKind::Image,
                    ordinal: next_ordinal,
                });
                next_ordinal += 1;
            }
            Event::Text(text) | Event::Code(text) => {
                for link in &mut pending {
                    append_label(link, &text);
                }
            }
            Event::SoftBreak | Event::HardBreak => {
                for link in &mut pending {
                    link.label.push(' ');
                }
            }
            Event::End(end @ (TagEnd::Link | TagEnd::Image)) => {
                let expected_image = end == TagEnd::Image;
                if let Some(index) = pending
                    .iter()
                    .rposition(|link| (link.kind == LinkKind::Image) == expected_image)
                {
                    links.push(pending.remove(index).into_link(source_path));
                }
            }
            _ => {}
        }
    }

    links.sort_by_key(|link| link.ordinal);
    links
}

fn append_label(link: &mut PendingLink, text: &CowStr<'_>) {
    link.label.push_str(text);
}

/// Whether a Markdown destination points outside the bundle.
///
/// A destination is external when it is protocol-relative (`//host/...`) or
/// carries an RFC 3986 scheme (`https:`, `mailto:`, `ftp:`, ...). Everything
/// else is treated as a candidate internal destination.
fn is_external_target(target: &str) -> bool {
    if target.starts_with("//") {
        return true;
    }
    target.split_once(':').is_some_and(|(scheme, _)| {
        let mut characters = scheme.chars();
        characters
            .next()
            .is_some_and(|first| first.is_ascii_alphabetic())
            && characters.all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '.' | '-'))
    })
}

fn classify(link_type: LinkType) -> LinkKind {
    match link_type {
        LinkType::Autolink => LinkKind::Autolink,
        LinkType::Email => LinkKind::Email,
        LinkType::Reference
        | LinkType::ReferenceUnknown
        | LinkType::Collapsed
        | LinkType::CollapsedUnknown
        | LinkType::Shortcut
        | LinkType::ShortcutUnknown
        | LinkType::WikiLink { .. } => LinkKind::Reference,
        LinkType::Inline => LinkKind::Inline,
    }
}

#[cfg(test)]
mod tests {
    use super::is_external_target;

    #[test]
    fn is_external_target_detects_schemes_and_protocol_relative() {
        assert!(is_external_target("https://example.test/page"));
        assert!(is_external_target("mailto:oncall@example.test"));
        assert!(is_external_target("ftp://files.example.test/x"));
        assert!(is_external_target("//cdn.example.test/asset.js"));
    }

    #[test]
    fn is_external_target_keeps_internal_destinations_internal() {
        assert!(!is_external_target("services/postgresql.md"));
        assert!(!is_external_target("../dashboards/health.md"));
        assert!(!is_external_target("/runbooks/failover.md"));
        assert!(!is_external_target("page.md#a:colon-in-fragment"));
    }
}
