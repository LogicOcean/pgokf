// SPDX-License-Identifier: AGPL-3.0-only
//! Killing tests for injection vectors that SURVIVE commit 9d53f98
//! ("close the adversarial review findings").
//!
//! Root cause of every exploitable case below:
//!
//! 1. quick-xml 0.41's `BytesText::decode()` deliberately skips XML 1.0
//!    end-of-line normalization (the normalizing variant is `xml_content()`),
//!    so a lone carriage return reaches the converter's text nodes both as a
//!    raw `\r` byte and via the character reference `&#13;` (handled by the
//!    `Event::GeneralRef` arm, which unescapes it back to `\r`).
//! 2. `escape_text` collapses `\r` (it is whitespace) and `inline_code`
//!    replaces it, but `raw_text` does not: fence bodies (`<pre>`,
//!    `code`/`noformat` macro bodies, the `script`/`style` fallback) keep a
//!    lone `\r` verbatim.
//! 3. The 9d53f98 fix taught `single_line` (headings, link labels) to split on
//!    `\r`, but the three container re-emitters - `quote()` (blockquotes and
//!    panel/note/info/tip/warning macros and the unsupported-macro fallback),
//!    `list()` (list items) and `table_cell()` (table cells) - all re-emit
//!    line-by-line using `str::lines()`, which splits ONLY on `\n`.
//! 4. A lone `\r` therefore lands inside an emitted line. CommonMark (and the
//!    catalog's parser, pulldown-cmark `Options::all()`) treats a lone `\r` as
//!    a line ending, so everything after it starts a NEW line at the
//!    container's outer level. Because the innermost open block is a fenced
//!    code block - which never supports lazy continuation - the container is
//!    closed there, and the remainder of the fence body, which is raw and
//!    unescaped, is parsed as live Markdown: raw HTML blocks,
//!    `javascript:` links, and relative `.md` destinations that okf-parser
//!    resolves as forged internal graph edges.
//!
//! These tests are written against the FIXED behavior: each asserts the
//! invariant the catalog relies on (no live HTML, only vetted https/mailto
//! destinations, no internal edges, and no `\r` surviving conversion). They
//! all FAIL against the current code on this branch; the pointwise expected
//! current emission is documented per fixture.
//!
//! Suggested fix (kills the whole class at the source): normalize end-of-line
//! characters in `parse_storage` - in the `Event::Text`, `Event::CData` and
//! `Event::GeneralRef` arms, replace `\r\n` with `\n` and any remaining lone
//! `\r` with `\n` (exactly what quick-xml's own `normalize_xml10_eols` does,
//! and what `BytesText::xml_content()` would have done). Every downstream
//! line-based emitter then splits the fence body correctly and the payload
//! stays inside the container. Alternatively normalize inside `fence()`.
//!
//! Drop this file in as crates/pgokf-confluence/tests/injection_survivors.rs.

use okf_parser::{ParsedConcept, ParserLimits, parse_concept};
use pgokf_confluence::convert::convert_pages;
use pgokf_confluence::types::{Document, Page, SourceIdentity};
use pulldown_cmark::{Event, Options, Parser, Tag};

fn source() -> SourceIdentity {
    SourceIdentity {
        site_url: "https://example.atlassian.net".to_owned(),
        space_id: "100".to_owned(),
    }
}

fn page(id: &str, title: &str, storage: &str) -> Page {
    Page {
        id: id.to_owned(),
        title: title.to_owned(),
        space_id: "100".to_owned(),
        parent_id: Some("1".to_owned()),
        version: 7,
        modified_at: Some("2026-11-06T10:00:00Z".to_owned()),
        storage: storage.to_owned(),
        web_url: format!("https://example.atlassian.net/wiki/spaces/AI/pages/{id}"),
    }
}

fn parsed(document: &Document) -> ParsedConcept {
    parse_concept(&document.bytes, &document.path, ParserLimits::default())
        .expect("converted document must be valid OKF")
}

fn text(document: &Document) -> &str {
    std::str::from_utf8(&document.bytes).expect("Markdown must be UTF-8")
}

/// Everything a Markdown renderer would make live in `markdown`: raw HTML
/// (block or inline) and link/image destinations, parsed exactly as the
/// catalog's own parser does (`Options::all()`).
fn live_markdown(markdown: &str) -> (Vec<String>, Vec<String>) {
    let mut html = Vec::new();
    let mut destinations = Vec::new();
    for event in Parser::new_ext(markdown, Options::all()) {
        match event {
            Event::Html(raw) | Event::InlineHtml(raw) => html.push(raw.into_string()),
            Event::Start(Tag::Link { dest_url, .. } | Tag::Image { dest_url, .. }) => {
                destinations.push(dest_url.into_string());
            }
            _ => {}
        }
    }
    (html, destinations)
}

fn convert_one(storage: &str) -> Document {
    convert_pages(&source(), &[page("50", "Untrusted", storage)])
        .unwrap()
        .remove(0)
}

/// Every fixture smuggles a lone carriage return into a fenced code body
/// (`&#13;`; a raw `\r` byte or a `\r` inside CDATA works the same) nested in
/// a container that re-emits content line-by-line on `\n` only.
const CR_BREAKOUTS: [&str; 8] = [
    // blockquote + <pre>: the fence body carries the raw <script> after a
    // lone \r. Current emission: "\n\n> ```\n> safe\r<script>alert(1)</script>\n> ```\n\n".
    // pulldown-cmark lines: ["> ```", "> safe", "<script>alert(1)</script>", "> ```"]:
    // the fenced code block inside the blockquote does not allow lazy
    // continuation, so line 3 closes the quote and is a live HTML block.
    "<blockquote><pre>safe&#13;&lt;script&gt;alert(1)&lt;/script&gt;</pre></blockquote>",
    // blockquote + <pre>, javascript: link variant. Current emission embeds
    // "> safe\r[click](javascript:alert(1))" -> live link, javascript: destination.
    "<blockquote><pre>safe&#13;[click](javascript:alert(1))</pre></blockquote>",
    // blockquote + <pre>, forged internal graph edge variant: a relative .md
    // destination that okf-parser resolves against the bundle (not external).
    "<blockquote><pre>safe&#13;[policy](../approved/policy.md)</pre></blockquote>",
    // blockquote + code macro (plain-text body, no CDATA needed).
    "<blockquote><ac:structured-macro ac:name=\"code\"><ac:plain-text-body>x&#13;[a](javascript:alert(2))</ac:plain-text-body></ac:structured-macro></blockquote>",
    // List item: continuation lines are indented by marker.len() spaces, but
    // the \r splits mid-line, so the payload continues at column 0, outside
    // the item. Current emission: "\n\n- ```\n  x\r[a](javascript:alert(9))\n  ```\n\n".
    "<ul><li><pre>x&#13;[a](javascript:alert(9))</pre></li></ul>",
    // Ordered variant with raw HTML.
    "<ol><li><pre>a&#13;&lt;script&gt;alert(3)&lt;/script&gt;</pre></li></ol>",
    // Table cell: table_cell() joins cell lines with "<br>" using lines()
    // (\n only), so a lone \r survives INSIDE the joined cell line and splits
    // the emitted row. A single \r is by luck wrapped into an accidental code
    // span by the cell's own fence markers; "\r\r" (blank line) breaks out
    // reliably. Current emission of the row:
    // "| h | ```<br>y\r\r[a](javascript:alert(7))<br>``` |\n| --- | --- |\n...".
    "<table><tr><td>h</td><td><pre>y&#13;&#13;[a](javascript:alert(7))</pre></td></tr><tr><td>d</td><td>d2</td></tr></table>",
    // panel/note/info/tip/warning macros (and the unsupported-macro fallback)
    // render through quote(): same breakout as the blockquote cases.
    "<ac:structured-macro ac:name=\"note\"><ac:rich-text-body><p>before</p><pre>p&#13;&lt;img src=x onerror=alert(1)&gt;</pre></ac:rich-text-body></ac:structured-macro>",
];

#[test]
fn lone_carriage_return_in_a_fenced_body_never_becomes_live_markdown() {
    for storage in CR_BREAKOUTS {
        // Arrange / Act
        let document = convert_one(storage);
        let (html, destinations) = live_markdown(text(&document));
        let concept = parsed(&document);

        // Assert: no lone carriage return may survive conversion at all.
        assert!(
            !text(&document).contains('\r'),
            "{storage}: emitted a lone carriage return: {:?}",
            text(&document)
        );
        // Assert: nothing may render as raw HTML.
        assert!(
            html.is_empty(),
            "{storage}: live HTML leaked into the catalog: {html:?}"
        );
        // Assert: every live link is a vetted absolute URL; no javascript:,
        // data: or relative (forged internal edge) destination.
        for destination in &destinations {
            assert!(
                destination.starts_with("https://"),
                "{storage}: unexpected live link {destination}"
            );
        }
        assert!(
            concept.links.iter().all(|link| link.is_external),
            "{storage}: forged internal edge {:?}",
            concept.links
        );
    }
}

/// The fix must not over-escape: a lone carriage return in a fenced body at
/// the TOP LEVEL (no container) is already held literally by the closing
/// fence, and the same content inside a heading or a link label is already
/// flattened by `single_line`. After normalizing `\r` to `\n` these must
/// keep working: the payload stays literal, the page stays parseable, and
/// the rest of the page (the trailing source link, the following paragraph)
/// stays live.
#[test]
fn carriage_returns_in_fenced_bodies_stay_literal_in_flat_contexts() {
    // Top-level <pre>: the fence holds the payload across the (now real) line
    // break; no HTML block is created.
    let storage =
        "<p>Before.</p><pre>safe&#13;&lt;script&gt;alert(1)&lt;/script&gt;</pre><p>After.</p>";
    let document = convert_one(storage);
    let (html, destinations) = live_markdown(text(&document));
    assert!(html.is_empty(), "{html:?}");
    assert!(
        destinations
            .iter()
            .any(|destination| destination.ends_with("/pages/50")),
        "{destinations:?}"
    );
    assert!(parsed(&document).body_text.contains("After."));
    assert!(parsed(&document).body_text.contains("alert(1)"));

    // Heading + <pre>: single_line flattens the fence into a code span.
    let storage = "<h2><pre>safe&#13;&lt;script&gt;alert(1)&lt;/script&gt;</pre></h2><p>After.</p>";
    let document = convert_one(storage);
    let (html, _) = live_markdown(text(&document));
    assert!(html.is_empty(), "{html:?}");
    assert!(parsed(&document).body_text.contains("After."));

    // Link label + <pre>: same flattening inside the label.
    let storage =
        "<p><a href=\"https://example.org/\"><pre>safe&#13;[a](javascript:alert(4))</pre></a></p>";
    let document = convert_one(storage);
    let (html, destinations) = live_markdown(text(&document));
    assert!(html.is_empty(), "{html:?}");
    for destination in &destinations {
        assert!(destination.starts_with("https://"), "{destination}");
    }

    // Blockquote + <pre> with a CRLF (already handled: lines() strips a
    // carriage return immediately before a newline) must stay contained too.
    let storage =
        "<blockquote><pre>safe&#13;&#10;&lt;script&gt;alert(1)&lt;/script&gt;</pre></blockquote>";
    let document = convert_one(storage);
    let (html, _) = live_markdown(text(&document));
    assert!(html.is_empty(), "{html:?}");
    assert!(parsed(&document).body_text.contains("alert(1)"));
}
