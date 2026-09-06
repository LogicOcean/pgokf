// SPDX-License-Identifier: AGPL-3.0-only
//! Markdown rendering and HTML sanitizing.
//!
//! Concept bodies are producer-authored Markdown and search headlines carry
//! `ts_headline` markup, so both pass through an allow-list sanitizer before
//! they reach a page: the renderer never trusts catalog content as HTML.

use std::collections::HashMap;

use ammonia::Builder;
use pulldown_cmark::{CowStr, Event, Options, Parser, Tag, TagEnd, html};

use crate::links::LinkTarget;

/// Render a Markdown body to sanitized HTML.
///
/// Tables, strikethrough, task lists, and footnotes are enabled (the common
/// GitHub-flavored subset OKF documents use); raw HTML in the source is
/// escaped by the sanitizer rather than passed through. Headings without an
/// explicit `{#id}` get a GitHub-style slug id so a document's own
/// table-of-contents anchors resolve.
pub(crate) fn render(markdown: &str) -> String {
    render_with_links(markdown, &|_| LinkTarget::Keep)
}

/// [`render`] with body links classified through `resolve`: a resolved
/// concept gets its page href, a dead concept path becomes plain marked-up
/// text, and anything else keeps its href. Images are left alone; the
/// sanitizer only admits same-origin images anyway (see [`sanitize`]).
pub(crate) fn render_with_links(markdown: &str, resolve: &dyn Fn(&str) -> LinkTarget) -> String {
    let mut options = Options::empty();
    options.insert(Options::ENABLE_TABLES);
    options.insert(Options::ENABLE_STRIKETHROUGH);
    options.insert(Options::ENABLE_TASKLISTS);
    options.insert(Options::ENABLE_FOOTNOTES);
    options.insert(Options::ENABLE_HEADING_ATTRIBUTES);
    let mut events = with_heading_ids(Parser::new_ext(markdown, options).collect());
    // Dead links are unwrapped into a marked span: the start tag is replaced
    // and the matching end tag (tracked by nesting depth) follows suit.
    let mut dead_depth: usize = 0;
    for event in &mut events {
        match event {
            Event::Start(Tag::Link { dest_url, .. }) => match resolve(dest_url) {
                LinkTarget::Page(href) => *dest_url = CowStr::from(href),
                LinkTarget::Dead => {
                    dead_depth += 1;
                    *event = Event::Html(CowStr::from(DEAD_LINK_OPEN));
                }
                LinkTarget::Keep => {}
            },
            Event::End(TagEnd::Link) if dead_depth > 0 => {
                dead_depth -= 1;
                *event = Event::Html(CowStr::from("</span>"));
            }
            _ => {}
        }
    }
    let mut unsafe_html = String::with_capacity(markdown.len() * 2);
    html::push_html(&mut unsafe_html, events.into_iter());
    sanitize(&unsafe_html)
}

/// The class the sanitizer admits on a `<span>`, and only this one, so a
/// body cannot borrow the page's own styling through raw HTML.
const DEAD_LINK_CLASS: &str = "unresolved";
const DEAD_LINK_OPEN: &str = "<span class=\"unresolved\" title=\"This link points at a document the catalog does not hold.\">";

/// The allow-list sanitizer every rendered body passes through. Beyond
/// ammonia's defaults (no scripts, event handlers, `javascript:` or `data:`
/// URLs) it keeps heading ids, marks links `noopener noreferrer`, and
/// refuses remote images so a body cannot make readers' browsers call out
/// to a third party with the page URL in the referrer.
fn sanitize(unsafe_html: &str) -> String {
    Builder::default()
        .add_generic_attributes(["id"])
        .add_tag_attributes("span", ["class", "title"])
        .link_rel(Some("noopener noreferrer"))
        .url_relative(ammonia::UrlRelative::PassThrough)
        .attribute_filter(|element, attribute, value| match (element, attribute) {
            ("img", "src") if !is_local_image(value) => None,
            ("span", "class") if value != DEAD_LINK_CLASS => None,
            _ => Some(value.into()),
        })
        .clean(unsafe_html)
        .to_string()
}

/// Same-origin images only: a relative or root-relative path, never a
/// scheme or a protocol-relative URL.
fn is_local_image(src: &str) -> bool {
    let src = src.trim();
    !src.starts_with("//") && !src.contains(':')
}

/// Give every heading that has no explicit id a slug of its text, numbered
/// `-1`, `-2`, ... on repeats, the way GitHub anchors headings.
fn with_heading_ids(mut events: Vec<Event<'_>>) -> Vec<Event<'_>> {
    let mut seen: HashMap<String, usize> = HashMap::new();
    for i in 0..events.len() {
        let Event::Start(Tag::Heading { id: None, .. }) = &events[i] else {
            continue;
        };
        let text: String = events[i + 1..]
            .iter()
            .take_while(|e| !matches!(e, Event::End(TagEnd::Heading(_))))
            .filter_map(|e| match e {
                Event::Text(t) | Event::Code(t) => Some(t.as_ref()),
                _ => None,
            })
            .collect();
        let base = slug(&text);
        let n = seen.entry(base.clone()).or_insert(0);
        let id = if *n == 0 {
            base.clone()
        } else {
            format!("{base}-{n}")
        };
        *n += 1;
        if let Event::Start(Tag::Heading { id: slot, .. }) = &mut events[i] {
            *slot = Some(CowStr::from(id));
        }
    }
    events
}

/// GitHub-style heading slug: lowercase, letters/digits/underscore kept,
/// spaces and hyphens become hyphens, everything else dropped.
fn slug(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.trim().chars() {
        if ch.is_alphanumeric() || ch == '_' {
            out.extend(ch.to_lowercase());
        } else if ch == ' ' || ch == '-' {
            out.push('-');
        }
    }
    if out.is_empty() {
        "section".to_owned()
    } else {
        out
    }
}

/// Drop a leading YAML frontmatter block (`---` ... `---`) from an OKF
/// concept file, leaving the Markdown body.
pub(crate) fn strip_frontmatter(source: &str) -> &str {
    let rest = source.strip_prefix("\u{feff}").unwrap_or(source);
    let Some(after_open) = rest.strip_prefix("---") else {
        return source;
    };
    let after_open = after_open.strip_prefix('\r').unwrap_or(after_open);
    let Some(after_open) = after_open.strip_prefix('\n') else {
        return source;
    };
    let mut offset = 0;
    for line in after_open.split_inclusive('\n') {
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if trimmed == "---" {
            return &after_open[offset + line.len()..];
        }
        offset += line.len();
    }
    source
}

/// Drop a leading ATX `# Heading` whose text equals `title`, so a page that
/// already shows the concept title as its header does not repeat it. Only
/// the first non-blank line is considered; anything else is returned intact.
pub(crate) fn strip_leading_title<'a>(body: &'a str, title: &str) -> &'a str {
    let trimmed = body.trim_start_matches(['\r', '\n']);
    let Some(first) = trimmed.lines().next() else {
        return body;
    };
    let Some(heading) = first.strip_prefix("# ") else {
        return body;
    };
    let heading = heading.trim().trim_end_matches('#').trim();
    if heading.eq_ignore_ascii_case(title.trim()) {
        &trimmed[first.len()..]
    } else {
        body
    }
}

/// Escape text for an HTML text node or attribute value (`&`, `<`, `>`,
/// `"`), the one escaper the hand-built SVG and the `linkify` filter share.
pub(crate) fn escape(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for ch in value.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            _ => out.push(ch),
        }
    }
    out
}

/// Sanitize a `ts_headline` snippet: keep only the `<b>` highlight markup the
/// database emits, escape everything else.
pub(crate) fn headline(snippet: &str) -> String {
    Builder::empty()
        .tags(std::collections::HashSet::from(["b"]))
        .clean(snippet)
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_with_links_rewrites_resolved_unwraps_dead_and_keeps_the_rest() {
        // Arrange
        let markdown = "[a](appendix.md) [b **bold**](gone.md) [c](https://x.example/)";
        let resolve = |href: &str| match href {
            "appendix.md" => LinkTarget::Page("/concepts/1/appendix".to_owned()),
            "gone.md" => LinkTarget::Dead,
            _ => LinkTarget::Keep,
        };

        // Act
        let html = render_with_links(markdown, &resolve);

        // Assert
        assert!(html.contains("href=\"/concepts/1/appendix\""), "{html}");
        assert!(!html.contains("gone.md"), "{html}");
        assert!(
            html.contains(
                "<span class=\"unresolved\" title=\"This link points at a document the catalog does not hold.\">b <strong>bold</strong></span>"
            ),
            "{html}"
        );
        assert!(html.contains("href=\"https://x.example/\""), "{html}");
    }

    #[test]
    fn sanitize_admits_only_the_dead_link_class_on_spans() {
        // Arrange
        let markdown = "<span class=\"pill ok\" title=\"t\">x</span>";

        // Act
        let html = render(markdown);

        // Assert
        assert!(html.contains("<span title=\"t\">x</span>"), "{html}");
    }

    #[test]
    fn sanitize_drops_remote_images_and_keeps_local_ones() {
        // Arrange
        let markdown = "![r](https://tracker.example/p.png) ![p](//cdn.example/p.png) ![l](/static/x.png) ![d](data:image/png;base64,AA==)";

        // Act
        let html = render(markdown);

        // Assert
        assert!(!html.contains("tracker.example"), "{html}");
        assert!(!html.contains("cdn.example"), "{html}");
        assert!(html.contains("src=\"/static/x.png\""), "{html}");
        assert!(!html.contains("data:image"), "{html}");
    }

    #[test]
    fn headline_keeps_only_bold_and_strips_nested_markup_and_attributes() {
        // Arrange
        let snippet = "<b onclick=\"x()\">hit</b> <i>it</i> <b><script>bad()</script>x</b> &amp; <a href=\"/\">link</a>";

        // Act
        let html = headline(snippet);

        // Assert
        assert_eq!(html, "<b>hit</b> it <b>x</b> &amp; link");
    }

    #[test]
    fn strip_frontmatter_handles_crlf_and_a_byte_order_mark() {
        // Arrange
        let crlf = "---\r\ntitle: A\r\n---\r\n\r\nBody\r\n";
        let bom = "\u{feff}---\ntitle: A\n---\nBody\n";

        // Act & Assert
        assert_eq!(strip_frontmatter(crlf), "\r\nBody\r\n");
        assert_eq!(strip_frontmatter(bom), "Body\n");
    }

    #[test]
    fn render_gives_headings_github_style_ids() {
        // Arrange
        let markdown =
            "## GUCs (run-time parameters)\n\n## Roles\n\n## Roles\n\n## Keep {#custom}\n";

        // Act
        let html = render(markdown);

        // Assert
        assert!(
            html.contains("<h2 id=\"gucs-run-time-parameters\">"),
            "{html}"
        );
        assert!(html.contains("<h2 id=\"roles\">"), "{html}");
        assert!(html.contains("<h2 id=\"roles-1\">"), "{html}");
        assert!(html.contains("<h2 id=\"custom\">"), "{html}");
    }

    #[test]
    fn strip_leading_title_removes_a_repeated_h1() {
        // Arrange
        let body = "\n# Failover runbook\n\nFirst paragraph.\n";

        // Act
        let stripped = strip_leading_title(body, "failover Runbook");

        // Assert
        assert_eq!(stripped, "\n\nFirst paragraph.\n");
    }

    #[test]
    fn strip_leading_title_keeps_a_different_or_later_heading() {
        // Arrange
        let different = "# Overview\n\nText.\n";
        let later = "Intro.\n\n# Failover runbook\n";

        // Act
        let kept_different = strip_leading_title(different, "Failover runbook");
        let kept_later = strip_leading_title(later, "Failover runbook");

        // Assert
        assert_eq!(kept_different, different);
        assert_eq!(kept_later, later);
    }

    #[test]
    fn render_produces_headings_and_lists() {
        // Arrange
        let markdown = "# Title\n\n- one\n- two\n";

        // Act
        let html = render(markdown);

        // Assert
        assert!(html.contains("<h1 id=\"title\">Title</h1>"), "{html}");
        assert!(html.contains("<li>one</li>"));
    }

    #[test]
    fn render_strips_scripts_and_event_handlers() {
        // Arrange: raw HTML embedded in Markdown.
        let markdown = "<script>alert(1)</script><a href=\"x\" onclick=\"evil()\">link</a>";

        // Act
        let html = render(markdown);

        // Assert
        assert!(!html.contains("<script"));
        assert!(!html.contains("onclick"));
        assert!(html.contains("<a href=\"x\" rel=\"noopener noreferrer\">link</a>"));
    }

    #[test]
    fn strip_frontmatter_removes_the_leading_yaml_block_only() {
        // Arrange
        let source = "---\ntype: Runbook\ntitle: A\n---\n\n# A\n\nbody --- not a fence\n";

        // Act
        let body = strip_frontmatter(source);

        // Assert
        assert_eq!(body, "\n# A\n\nbody --- not a fence\n");
        assert_eq!(
            strip_frontmatter("# no frontmatter\n"),
            "# no frontmatter\n"
        );
        assert_eq!(
            strip_frontmatter("---\nunterminated\n"),
            "---\nunterminated\n"
        );
    }

    #[test]
    fn escape_handles_the_four_html_specials() {
        // Arrange & Act & Assert
        assert_eq!(escape("a<b>&\"c\""), "a&lt;b&gt;&amp;&quot;c&quot;");
    }

    #[test]
    fn headline_keeps_bold_and_escapes_the_rest() {
        // Arrange
        let snippet = "the <b>peregrine</b> <i>strategy</i> <img src=x onerror=1>";

        // Act
        let html = headline(snippet);

        // Assert
        assert_eq!(html, "the <b>peregrine</b> strategy ");
    }
}
