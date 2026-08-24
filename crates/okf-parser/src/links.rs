use pulldown_cmark::{CowStr, Event, LinkType, Options, Parser, Tag, TagEnd};
use serde::{Deserialize, Serialize};

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
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Link {
    pub target: String,
    pub label: String,
    pub kind: LinkKind,
    pub ordinal: usize,
}

struct PendingLink {
    target: String,
    label: String,
    kind: LinkKind,
    ordinal: usize,
}

/// Extract Markdown links and images while preserving document order.
#[must_use]
pub fn extract(markdown: &str) -> Vec<Link> {
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
                    let link = pending.remove(index);
                    links.push(Link {
                        target: link.target,
                        label: link.label.trim().to_owned(),
                        kind: link.kind,
                        ordinal: link.ordinal,
                    });
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
