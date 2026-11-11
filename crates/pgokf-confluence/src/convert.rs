// SPDX-License-Identifier: AGPL-3.0-only
//! Deterministic conversion of Confluence storage fragments into OKF documents.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;

use anyhow::{Context, Result, bail, ensure};
use quick_xml::events::{BytesStart, Event};
use quick_xml::{Reader, XmlVersion};
use serde_json::json;
use url::Url;

use crate::types::{Document, Page, SourceIdentity};

const MAX_DEPTH: usize = 128;
const MAX_NODES: usize = 200_000;
const MAX_STORAGE_BYTES: usize = 4 * 1024 * 1024;
// Match the default OKF file-size ceiling, and bound sparse-table expansion
// independently of the input byte/node limits. The cell budget is per page and
// includes padding and the Markdown header separator row of every table.
const MAX_RENDERED_BYTES: usize = 4 * 1024 * 1024;
const MAX_TABLE_CELLS: usize = 100_000;

fn push_output(output: &mut String, text: &str) -> Result<()> {
    ensure!(
        output
            .len()
            .checked_add(text.len())
            .is_some_and(|size| size <= MAX_RENDERED_BYTES),
        "rendered page exceeds conversion byte limit"
    );
    output.push_str(text);
    Ok(())
}

#[derive(Debug)]
enum Node {
    Text(String),
    Element(Element),
}

#[derive(Debug, Default)]
struct Element {
    name: String,
    attrs: BTreeMap<String, String>,
    children: Vec<Node>,
}

impl Element {
    fn attr(&self, name: &str) -> Option<&str> {
        self.attrs.get(name).map(String::as_str)
    }

    fn child(&self, name: &str) -> Option<&Self> {
        self.children.iter().find_map(|node| match node {
            Node::Element(element) if element.name == name => Some(element),
            _ => None,
        })
    }

    fn parameter(&self, name: &str) -> Option<String> {
        self.children.iter().find_map(|node| match node {
            Node::Element(element)
                if element.name == "ac:parameter" && element.attr("ac:name") == Some(name) =>
            {
                Some(raw_text(&element.children))
            }
            _ => None,
        })
    }
}

fn xml_element(start: &BytesStart<'_>, reader: &Reader<&[u8]>) -> Result<Element> {
    let name = std::str::from_utf8(start.name().as_ref())?.to_owned();
    let attrs = start
        .attributes()
        .map(|attr| {
            let attr = attr?;
            Ok((
                std::str::from_utf8(attr.key.as_ref())?.to_owned(),
                attr.decoded_and_normalized_value(XmlVersion::Implicit1_0, reader.decoder())?
                    .into_owned(),
            ))
        })
        .collect::<Result<_>>()?;
    Ok(Element {
        name,
        attrs,
        children: Vec::new(),
    })
}

fn push_text(parent: &mut Element, text: &str) {
    if let Some(Node::Text(previous)) = parent.children.last_mut() {
        previous.push_str(text);
    } else {
        parent.children.push(Node::Text(text.to_owned()));
    }
}

fn parse_storage(storage: &str) -> Result<Vec<Node>> {
    ensure!(
        storage.len() <= MAX_STORAGE_BYTES,
        "storage body exceeds conversion byte limit"
    );
    // An explicitly present empty storage string is a valid blank Confluence
    // page. Missing body/storage/value fields are rejected by the API client.
    // The renderer preserves blank pages with a source link and visible warning.
    let mut reader = Reader::from_str(storage);
    let mut stack = vec![Element::default()];
    let mut events = 0;
    loop {
        events += 1;
        ensure!(
            events <= MAX_NODES,
            "storage body exceeds conversion node limit"
        );
        match reader
            .read_event()
            .context("invalid Confluence storage XHTML")?
        {
            Event::Start(start) => {
                ensure!(
                    stack.len() < MAX_DEPTH,
                    "storage body exceeds conversion depth limit"
                );
                stack.push(xml_element(&start, &reader)?);
            }
            Event::Empty(start) => {
                let element = xml_element(&start, &reader)?;
                stack
                    .last_mut()
                    .context("missing XML root")?
                    .children
                    .push(Node::Element(element));
            }
            Event::End(_) => {
                ensure!(stack.len() > 1, "unexpected XML closing tag");
                let element = stack.pop().context("missing XML element")?;
                stack
                    .last_mut()
                    .context("missing XML root")?
                    .children
                    .push(Node::Element(element));
            }
            Event::Text(text) => {
                push_text(
                    stack.last_mut().context("missing XML root")?,
                    &text.decode()?,
                );
            }
            Event::CData(text) => {
                push_text(
                    stack.last_mut().context("missing XML root")?,
                    &text.decode()?,
                );
            }
            Event::GeneralRef(reference) => {
                let encoded = format!("&{};", reference.decode()?);
                let decoded = quick_xml::escape::unescape(&encoded)
                    .context("unknown or invalid storage entity")?;
                push_text(stack.last_mut().context("missing XML root")?, &decoded);
            }
            Event::DocType(_) => bail!("DTD declarations are not supported in Confluence storage"),
            Event::Decl(_) | Event::PI(_) => {
                bail!("XML declarations and processing instructions are not storage fragments")
            }
            Event::Comment(_) => {}
            Event::Eof => break,
        }
    }
    ensure!(stack.len() == 1, "unclosed Confluence storage element");
    Ok(stack.pop().context("missing XML root")?.children)
}

fn raw_text(nodes: &[Node]) -> String {
    nodes
        .iter()
        .map(|node| match node {
            Node::Text(text) => text.clone(),
            Node::Element(element) => raw_text(&element.children),
        })
        .collect()
}

fn collapse_whitespace(text: &str) -> String {
    let mut result = String::new();
    let mut space = false;
    for character in text.chars() {
        if character.is_whitespace() {
            if !space {
                result.push(' ');
            }
            space = true;
        } else {
            result.push(character);
            space = false;
        }
    }
    result
}

fn escape_text(text: &str) -> String {
    let mut escaped = String::new();
    for character in collapse_whitespace(text).chars() {
        match character {
            '&' => escaped.push_str("&amp;"),
            '\\' | '`' | '*' | '_' | '[' | ']' | '<' | '>' | '#' | '|' | '!' | '+' | '-' | '.'
            | '~' => {
                escaped.push('\\');
                escaped.push(character);
            }
            _ => escaped.push(character),
        }
    }
    escaped
}

fn block(text: &str) -> String {
    if text.trim().is_empty() {
        String::new()
    } else {
        format!("\n\n{}\n\n", text.trim())
    }
}

fn quote(text: &str) -> String {
    block(
        &text
            .trim()
            .lines()
            .map(|line| format!("> {line}"))
            .collect::<Vec<_>>()
            .join("\n"),
    )
}

fn fence(text: &str, language: &str) -> String {
    let length = text
        .split(|character| character != '`')
        .map(str::len)
        .max()
        .unwrap_or(0)
        + 1;
    let delimiter = "`".repeat(length.max(3));
    let language = language
        .chars()
        .filter(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '+')
        })
        .collect::<String>();
    format!(
        "\n\n{delimiter}{language}\n{}{delimiter}\n\n",
        if text.ends_with('\n') {
            text.to_owned()
        } else {
            format!("{text}\n")
        }
    )
}

fn inline_code(text: &str) -> String {
    let length = text
        .split(|character| character != '`')
        .map(str::len)
        .max()
        .unwrap_or(0)
        + 1;
    let delimiter = "`".repeat(length);
    // Padding is removed by CommonMark; it protects literal edge backticks.
    format!("{delimiter} {text} {delimiter}")
}

fn link(label: &str, target: &str) -> String {
    let target = target
        .replace('&', "&amp;")
        .replace('<', "%3C")
        .replace('>', "%3E")
        .replace(['\r', '\n'], "");
    format!("[{}](<{target}>)", label.trim())
}

struct PageIndex<'a> {
    source: &'a SourceIdentity,
    wiki_url: Url,
    pages: BTreeMap<&'a str, &'a Page>,
    titles: BTreeMap<&'a str, Option<&'a Page>>,
    space_keys: BTreeSet<String>,
}

impl<'a> PageIndex<'a> {
    fn new(source: &'a SourceIdentity, pages: &'a [Page]) -> Result<Self> {
        let mut wiki_url = Url::parse(&source.site_url).context("invalid Confluence site URL")?;
        ensure!(
            matches!(wiki_url.scheme(), "http" | "https")
                && wiki_url.username().is_empty()
                && wiki_url.password().is_none(),
            "Confluence site URL must be an HTTP URL without credentials"
        );
        let base_path = wiki_url.path().trim_end_matches('/');
        let wiki_path = if base_path.ends_with("/wiki") {
            format!("{base_path}/")
        } else {
            format!("{base_path}/wiki/")
        };
        wiki_url.set_path(&wiki_path);
        wiki_url.set_query(None);
        wiki_url.set_fragment(None);
        let mut index = Self {
            source,
            wiki_url,
            pages: BTreeMap::new(),
            titles: BTreeMap::new(),
            space_keys: BTreeSet::new(),
        };
        for page in pages {
            ensure!(
                !page.id.is_empty() && page.id.bytes().all(|byte| byte.is_ascii_digit()),
                "Confluence page ID must contain only digits"
            );
            ensure!(
                page.space_id == source.space_id,
                "page {} belongs to a different Confluence space",
                page.id
            );
            ensure!(
                index.pages.insert(&page.id, page).is_none(),
                "duplicate Confluence page ID {}",
                page.id
            );
            ensure!(
                !page.title.trim().is_empty(),
                "page {} has no title",
                page.id
            );
            let canonical =
                Url::parse(&page.web_url).context("invalid canonical Confluence page URL")?;
            ensure!(
                matches!(canonical.scheme(), "http" | "https")
                    && canonical.username().is_empty()
                    && canonical.password().is_none(),
                "canonical Confluence page URL must be HTTP without credentials"
            );
            ensure!(
                canonical.origin() == index.wiki_url.origin(),
                "canonical page URL belongs to a different Confluence site"
            );
            index
                .titles
                .entry(&page.title)
                .and_modify(|entry| *entry = None)
                .or_insert(Some(page));
            // Cloud web UI URLs provide the space key even though REST v2 uses a numeric space ID.
            let segments = canonical
                .path_segments()
                .map(Iterator::collect::<Vec<_>>)
                .unwrap_or_default();
            if let Some(position) = segments.iter().position(|segment| *segment == "spaces")
                && let Some(key) = segments.get(position + 1)
            {
                index.space_keys.insert((*key).to_owned());
            }
        }
        Ok(index)
    }

    fn page_url(&self, id: &str) -> String {
        let mut url = self.wiki_url.clone();
        url.set_path(&format!("{}pages/viewpage.action", url.path()));
        url.query_pairs_mut().append_pair("pageId", id);
        url.into()
    }

    fn lookup_title(&self, title: &str, key: Option<&str>) -> Option<&Page> {
        if key.is_some_and(|key| !self.space_keys.contains(key)) {
            return None;
        }
        self.titles.get(title).copied().flatten()
    }

    fn title_url(&self, title: &str, key: Option<&str>) -> String {
        let mut url = self.wiki_url.clone();
        url.set_path(&format!("{}pages/viewpage.action", url.path()));
        url.query_pairs_mut().append_pair("title", title);
        let key = key.or_else(|| {
            (self.space_keys.len() == 1)
                .then(|| self.space_keys.first().map(String::as_str))
                .flatten()
        });
        if let Some(key) = key {
            url.query_pairs_mut().append_pair("spaceKey", key);
        } else {
            url.query_pairs_mut()
                .append_pair("spaceId", &self.source.space_id);
        }
        url.into()
    }
}

struct Renderer<'a, 'index> {
    page: &'a Page,
    index: &'index PageIndex<'a>,
    warnings: BTreeSet<String>,
    table_cells: usize,
}

impl Renderer<'_, '_> {
    fn warn(&mut self, message: impl Into<String>) {
        self.warnings.insert(message.into());
    }

    fn children(&mut self, children: &[Node]) -> Result<String> {
        let mut output = String::new();
        for child in children {
            push_output(&mut output, &self.node(child)?)?;
        }
        Ok(output)
    }

    fn node(&mut self, node: &Node) -> Result<String> {
        let output = match node {
            Node::Text(text) => escape_text(text),
            Node::Element(element) => self.element(element)?,
        };
        ensure!(
            output.len() <= MAX_RENDERED_BYTES,
            "rendered page exceeds conversion byte limit"
        );
        Ok(output)
    }

    #[allow(clippy::too_many_lines)]
    fn element(&mut self, element: &Element) -> Result<String> {
        Ok(match element.name.as_str() {
            // Confluence Cloud's column layouts (ac:layout > ac:layout-section
            // > ac:layout-cell) are purely structural: their content renders
            // as ordinary blocks, and an empty cell renders nothing.
            "p" | "div" | "section" | "ac:rich-text-body" | "ac:layout" | "ac:layout-section"
            | "ac:layout-cell" => block(&self.children(&element.children)?),
            // Transparent containers. Underline has no Markdown form, so its
            // text is kept like a span's; a list item reached outside its list
            // (lists render their own items) keeps its content.
            "span"
            | "u"
            | "li"
            | "tbody"
            | "thead"
            | "tfoot"
            | "ac:link-body"
            | "ac:plain-text-link-body" => self.children(&element.children)?,
            "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => {
                let level = usize::from(element.name.as_bytes()[1] - b'0');
                block(&format!(
                    "{} {}",
                    "#".repeat(level),
                    self.children(&element.children)?.trim()
                ))
            }
            "strong" | "b" => format!("**{}**", self.children(&element.children)?),
            "em" | "i" => format!("*{}*", self.children(&element.children)?),
            "s" | "del" | "strike" => format!("~~{}~~", self.children(&element.children)?),
            "code" | "tt" => inline_code(&raw_text(&element.children)),
            "pre" => fence(&raw_text(&element.children), ""),
            "blockquote" => quote(&self.children(&element.children)?),
            "br" => "  \n".to_owned(),
            "hr" => block("---"),
            "ul" | "ol" => self.list(element)?,
            "table" => self.table(element)?,
            "a" => {
                let body = self.children(&element.children)?;
                element.attr("href").map_or(body.clone(), |href| {
                    let target = self.href(href);
                    link(
                        if body.trim().is_empty() {
                            "Link"
                        } else {
                            &body
                        },
                        &target,
                    )
                })
            }
            "ac:link" => self.confluence_link(element)?,
            "ac:image" => self.image(element),
            "img" => {
                let label = escape_text(element.attr("alt").unwrap_or("Image"));
                if let Some(src) = element.attr("src") {
                    let target = self.href(src);
                    link(&label, &target)
                } else {
                    self.warn("Image without a source; see the original Confluence page");
                    self.source_link()
                }
            }
            "ac:structured-macro" | "ac:macro" => self.macro_element(element)?,
            "ri:page" => {
                let (label, target) = self.page_reference(element);
                link(&escape_text(&label), &target)
            }
            "ri:attachment" => self.attachment(element),
            "ri:url" => {
                let href = element
                    .attr("ri:value")
                    .unwrap_or(&self.page.web_url)
                    .to_owned();
                let target = self.href(&href);
                link(&escape_text(&href), &target)
            }
            "ac:emoticon" => escape_text(element.attr("ac:name").unwrap_or("emoticon")),
            "script" | "style" => {
                self.warn(format!(
                    "Unsupported {} element preserved as literal text",
                    element.name
                ));
                fence(&raw_text(&element.children), "text")
            }
            _ => {
                self.warn(format!(
                    "Unsupported storage element {}; text retained",
                    element.name
                ));
                let body = self.children(&element.children)?;
                if body.trim().is_empty() {
                    link(
                        &format!("Unsupported {}", escape_text(&element.name)),
                        &self.page.web_url,
                    )
                } else {
                    body
                }
            }
        })
    }

    fn source_link(&self) -> String {
        link("View Confluence source", &self.page.web_url)
    }

    fn href(&mut self, href: &str) -> String {
        let Ok(base) = Url::parse(&self.page.web_url) else {
            return self.page.web_url.clone();
        };
        let Ok(url) = base.join(href) else {
            self.warn("Malformed link replaced with the source page URL");
            return self.page.web_url.clone();
        };
        if !matches!(url.scheme(), "http" | "https" | "mailto")
            || !url.username().is_empty()
            || url.password().is_some()
        {
            self.warn("Unsupported link scheme or embedded credentials; see the original page");
            return self.page.web_url.clone();
        }
        if url.origin() == self.index.wiki_url.origin() {
            let query_id = url
                .query_pairs()
                .find_map(|(key, value)| (key == "pageId").then(|| value.into_owned()));
            let segments = url
                .path_segments()
                .map(Iterator::collect::<Vec<_>>)
                .unwrap_or_default();
            let path_id = segments.windows(2).find_map(|pair| {
                (pair[0] == "pages" && pair[1].bytes().all(|byte| byte.is_ascii_digit()))
                    .then(|| pair[1].to_owned())
            });
            if let Some(id) = query_id.or(path_id)
                && self.index.pages.contains_key(id.as_str())
            {
                if url.fragment().is_some() {
                    self.warn("Confluence section anchor replaced with a page-level internal link");
                }
                return format!("{id}.md");
            }
            let title = url
                .query_pairs()
                .find_map(|(key, value)| (key == "title").then(|| value.into_owned()));
            let space_key = url
                .query_pairs()
                .find_map(|(key, value)| (key == "spaceKey").then(|| value.into_owned()));
            if let Some(title) = title
                && let Some(page) = self.index.lookup_title(&title, space_key.as_deref())
            {
                return format!("{}.md", page.id);
            }
        }
        url.into()
    }

    fn page_reference(&mut self, reference: &Element) -> (String, String) {
        let title = reference.attr("ri:content-title");
        if let Some(id) = reference.attr("ri:content-id") {
            if let Some(page) = self.index.pages.get(id) {
                return (title.unwrap_or(&page.title).to_owned(), format!("{id}.md"));
            }
            return (title.unwrap_or(id).to_owned(), self.index.page_url(id));
        }
        if let Some(title) = title {
            let key = reference.attr("ri:space-key");
            if let Some(page) = self.index.lookup_title(title, key) {
                return (title.to_owned(), format!("{}.md", page.id));
            }
            self.warn(format!(
                "Page title reference could not be resolved uniquely in this import: {}",
                collapse_whitespace(title)
            ));
            return (title.to_owned(), self.index.title_url(title, key));
        }
        self.warn("Confluence page reference has neither an ID nor a title");
        ("Unresolved page".to_owned(), self.page.web_url.clone())
    }

    fn confluence_link(&mut self, element: &Element) -> Result<String> {
        let label = element
            .child("ac:link-body")
            .or_else(|| element.child("ac:plain-text-link-body"))
            .map(|body| self.children(&body.children))
            .transpose()?;
        if let Some(reference) = element.child("ri:page") {
            let (title, target) = self.page_reference(reference);
            if element.attr("ac:anchor").is_some() {
                self.warn("Confluence section anchor replaced with a page-level internal link");
            }
            return Ok(link(&label.unwrap_or_else(|| escape_text(&title)), &target));
        }
        if let Some(attachment) = element.child("ri:attachment") {
            let target = self.attachment_url(attachment);
            return Ok(link(
                &label.unwrap_or_else(|| {
                    escape_text(attachment.attr("ri:filename").unwrap_or("Attachment"))
                }),
                &target,
            ));
        }
        if let Some(reference) = element.child("ri:url") {
            let href = reference
                .attr("ri:value")
                .unwrap_or(&self.page.web_url)
                .to_owned();
            let target = self.href(&href);
            return Ok(link(&label.unwrap_or_else(|| escape_text(&href)), &target));
        }
        self.warn("Unresolved Confluence link retained with the original page URL");
        Ok(link(
            &label.unwrap_or_else(|| "View Confluence link".to_owned()),
            &self.page.web_url,
        ))
    }

    fn attachment_url(&mut self, element: &Element) -> String {
        let Some(filename) = element.attr("ri:filename") else {
            self.warn("Attachment has no filename; see the original page");
            return self.page.web_url.clone();
        };
        let mut page_id = self.page.id.clone();
        if let Some(reference) = element.child("ri:page") {
            if let Some(id) = reference.attr("ri:content-id") {
                id.clone_into(&mut page_id);
            } else if let Some(page) = reference.attr("ri:content-title").and_then(|title| {
                self.index
                    .lookup_title(title, reference.attr("ri:space-key"))
            }) {
                page_id.clone_from(&page.id);
            } else {
                self.warn("Attachment owner could not be resolved; see the original page");
                return self.page.web_url.clone();
            }
        }
        let mut url = self.index.wiki_url.clone();
        if let Ok(mut segments) = url.path_segments_mut() {
            segments
                .pop_if_empty()
                .extend(["download", "attachments", &page_id, filename]);
        }
        url.into()
    }

    fn attachment(&mut self, element: &Element) -> String {
        let target = self.attachment_url(element);
        link(
            &escape_text(element.attr("ri:filename").unwrap_or("Attachment")),
            &target,
        )
    }

    fn image(&mut self, element: &Element) -> String {
        if let Some(attachment) = element.child("ri:attachment") {
            return self.attachment(attachment);
        }
        if let Some(reference) = element.child("ri:url") {
            let href = reference
                .attr("ri:value")
                .unwrap_or(&self.page.web_url)
                .to_owned();
            let target = self.href(&href);
            return link(
                &escape_text(element.attr("ac:alt").unwrap_or("Image")),
                &target,
            );
        }
        self.warn("Unsupported image reference; see the original Confluence page");
        self.source_link()
    }

    fn macro_element(&mut self, element: &Element) -> Result<String> {
        let name = element.attr("ac:name").unwrap_or("unnamed");
        let body = element
            .child("ac:rich-text-body")
            .or_else(|| element.child("ac:plain-text-body"));
        Ok(match name {
            "code" | "noformat" => {
                let text = body.map_or_else(String::new, |body| raw_text(&body.children));
                fence(&text, &element.parameter("language").unwrap_or_default())
            }
            "panel" | "note" | "info" | "tip" | "warning" => {
                let title = element
                    .parameter("title")
                    .unwrap_or_else(|| name.to_owned());
                let content = body
                    .map(|body| self.children(&body.children))
                    .transpose()?
                    .unwrap_or_default();
                quote(&format!(
                    "**{}**\n\n{}",
                    escape_text(&title),
                    content.trim()
                ))
            }
            _ => {
                self.warn(format!(
                    "Unsupported dynamic macro {name}; rendered content may be incomplete"
                ));
                let mut fallback = format!(
                    "Unsupported Confluence macro: {}. {}",
                    escape_text(name),
                    self.source_link()
                );
                if let Some(body) = body {
                    // Unknown macro parameters are operational configuration, not page prose.
                    // Only the explicitly supplied text body is retained, as a quoted fallback.
                    let text = self.children(&body.children)?;
                    if !text.trim().is_empty() {
                        let _ = write!(fallback, "\n\n{}", text.trim());
                    }
                }
                quote(&fallback)
            }
        })
    }

    fn list(&mut self, element: &Element) -> Result<String> {
        let ordered = element.name == "ol";
        let start = element
            .attr("start")
            .and_then(|value| value.parse::<usize>().ok())
            .unwrap_or(1);
        let mut output = String::new();
        let mut count = 0;
        for child in &element.children {
            let item = match child {
                Node::Element(item) if item.name == "li" => {
                    let marker = if ordered {
                        format!("{}. ", start.saturating_add(count))
                    } else {
                        "- ".to_owned()
                    };
                    let content = self.children(&item.children)?;
                    let mut lines = content.trim().lines();
                    let mut rendered = marker.clone();
                    push_output(&mut rendered, lines.next().unwrap_or_default())?;
                    for line in lines {
                        push_output(&mut rendered, "\n")?;
                        push_output(&mut rendered, &" ".repeat(marker.len()))?;
                        push_output(&mut rendered, line)?;
                    }
                    rendered
                }
                Node::Text(text) if text.trim().is_empty() => continue,
                _ => {
                    self.warn("Unexpected list child retained as an additional list item");
                    format!("- {}", self.node(child)?.trim())
                }
            };
            if count > 0 {
                push_output(&mut output, "\n")?;
            }
            push_output(&mut output, &item)?;
            count += 1;
        }
        Ok(block(&output))
    }

    fn table(&mut self, element: &Element) -> Result<String> {
        // Collect references to actual cells only. Never create an owned,
        // rectangular matrix: a sparse table can have width * rows vastly
        // larger than its source. Check the complete shape before rendering.
        let mut rows = Vec::new();
        let mut extras = Vec::new();
        table_rows(&element.children, &mut rows, &mut extras);
        let width = rows.iter().map(Vec::len).max().unwrap_or(0);
        if width == 0 {
            self.warn("Table has no cells; fallback text retained");
            return Ok(block(&self.children(&element.children)?));
        }
        self.table_cells = rows
            .len()
            .checked_add(1)
            .and_then(|height| width.checked_mul(height))
            .and_then(|cells| self.table_cells.checked_add(cells))
            .filter(|cells| *cells <= MAX_TABLE_CELLS)
            .context("page exceeds conversion table cell limit")?;

        let mut output = String::new();
        push_output(&mut output, "\n\n")?;
        for (index, row) in rows.iter().enumerate() {
            push_output(&mut output, "| ")?;
            for column in 0..width {
                if column > 0 {
                    push_output(&mut output, " | ")?;
                }
                if let Some(cell) = row.get(column) {
                    if cell.attr("rowspan").is_some_and(|value| value != "1")
                        || cell.attr("colspan").is_some_and(|value| value != "1")
                    {
                        self.warn("Table cell spans flattened; see source for the original layout");
                    }
                    let rendered = self.children(&cell.children)?;
                    push_output(&mut output, &table_cell(&rendered))?;
                }
            }
            push_output(&mut output, " |\n")?;
            if index == 0 {
                push_output(&mut output, "| ")?;
                for column in 0..width {
                    if column > 0 {
                        push_output(&mut output, " | ")?;
                    }
                    push_output(&mut output, "---")?;
                }
                push_output(&mut output, " |\n")?;
            }
        }
        push_output(&mut output, "\n")?;
        for extra in extras {
            push_output(&mut output, &self.node(extra)?)?;
        }
        Ok(output)
    }
}

fn table_rows<'a>(nodes: &'a [Node], rows: &mut Vec<Vec<&'a Element>>, extras: &mut Vec<&'a Node>) {
    for node in nodes {
        match node {
            Node::Element(element)
                if matches!(element.name.as_str(), "thead" | "tbody" | "tfoot") =>
            {
                table_rows(&element.children, rows, extras);
            }
            Node::Element(element) if element.name == "tr" => {
                let mut cells = Vec::new();
                for cell in &element.children {
                    match cell {
                        Node::Element(cell) if matches!(cell.name.as_str(), "td" | "th") => {
                            cells.push(cell);
                        }
                        Node::Text(text) if text.trim().is_empty() => {}
                        _ => extras.push(cell),
                    }
                }
                rows.push(cells);
            }
            Node::Element(element) if matches!(element.name.as_str(), "colgroup" | "col") => {}
            Node::Text(text) if text.trim().is_empty() => {}
            _ => extras.push(node),
        }
    }
}

fn table_cell(text: &str) -> String {
    let mut escaped = String::new();
    let mut slashes = 0;
    for character in text.trim().chars() {
        if character == '|' && slashes % 2 == 0 {
            escaped.push('\\');
        }
        escaped.push(character);
        if character == '\\' {
            slashes += 1;
        } else {
            slashes = 0;
        }
    }
    escaped
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join("<br>")
}

fn frontmatter(source: &SourceIdentity, page: &Page) -> Result<String> {
    // JSON values are valid YAML flow values. Escaped single-line values prevent
    // source titles from introducing delimiters, YAML keys, or trust metadata.
    let fields = [
        ("type", json!("Document")),
        ("title", json!(page.title)),
        ("resource", json!({"url": page.web_url})),
        ("tags", json!(["confluence"])),
        (
            "confluence",
            json!({"site_url": source.site_url, "cloud_id": source.cloud_id, "space_id": source.space_id, "page_id": page.id, "parent_id": page.parent_id, "version": page.version, "modified_at": page.modified_at}),
        ),
        ("converter_version", json!(env!("CARGO_PKG_VERSION"))),
        (
            "generated",
            json!({"by": format!("pgokf-confluence/{}", env!("CARGO_PKG_VERSION"))}),
        ),
    ];
    let mut output = "---\n".to_owned();
    for (name, value) in fields {
        writeln!(output, "{name}: {}", serde_json::to_string(&value)?)?;
    }
    output.push_str("---\n\n");
    Ok(output)
}

/// Convert a complete, single-space snapshot to stable OKF Markdown documents.
///
/// Returned documents are sorted by numeric-ID string. Malformed bodies
/// fail the snapshot; unsupported constructs retain a source reference and emit
/// deterministic warnings. Source content never supplies verification metadata.
///
/// # Errors
/// Returns an error for invalid identities, duplicate IDs, missing titles,
/// unsafe document paths, invalid or excessively nested XML, table/output budget
/// exhaustion, and serialization.
pub fn convert_pages(source: &SourceIdentity, pages: &[Page]) -> Result<Vec<Document>> {
    let index = PageIndex::new(source, pages)?;
    index
        .pages
        .values()
        .map(|page| {
            let nodes = parse_storage(&page.storage)
                .with_context(|| format!("convert Confluence page {}", page.id))?;
            let mut renderer = Renderer {
                page,
                index: &index,
                warnings: BTreeSet::new(),
                table_cells: 0,
            };
            let mut body = renderer
                .children(&nodes)
                .with_context(|| format!("convert Confluence page {}", page.id))?;
            if body.trim().is_empty() {
                renderer.warn(
                    "Storage body contains no visible content; see the original Confluence page",
                );
                "This Confluence page has no visible storage content.".clone_into(&mut body);
            }
            let mut output = frontmatter(source, page)?;
            for part in [
                "# ",
                escape_text(&page.title).trim(),
                "\n\n",
                body.trim(),
                "\n\nSource: ",
                &renderer.source_link(),
                "\n\n",
            ] {
                push_output(&mut output, part)
                    .with_context(|| format!("convert Confluence page {}", page.id))?;
            }
            Ok(Document {
                path: format!("pages/{}.md", page.id),
                bytes: output.into_bytes(),
                page_id: page.id.clone(),
                version: page.version,
                warnings: renderer.warnings.into_iter().collect(),
            })
        })
        .collect()
}
