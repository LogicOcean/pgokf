// SPDX-License-Identifier: AGPL-3.0-only
use okf_parser::{ParsedConcept, ParserLimits, parse_concept};
use pgokf_confluence::convert::convert_pages;
use pgokf_confluence::types::{Document, Page, SourceIdentity};

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

#[test]
fn rich_storage_preserves_entities_tables_code_lists_and_notes() {
    let pages = [page(
        "10",
        "Reference",
        include_str!("fixtures/rich-storage.xml"),
    )];
    let documents = convert_pages(&source(), &pages).unwrap();
    let document = &documents[0];
    let markdown = text(document);
    let concept = parsed(document);
    assert_eq!(concept.r#type, "Document");
    assert!(document.warnings.is_empty(), "{:?}", document.warnings);
    assert!(markdown.contains("| A \\| B | ` a\\|b `<br>next line |"));
    assert!(markdown.contains("````rust\nlet example = \"```\";"));
    assert!(markdown.contains("3. Third item\n4. Fourth item"));
    assert!(markdown.contains("  - Nested item"));
    assert!(markdown.contains("> **Read first**"));
    assert!(
        concept
            .body_text
            .contains("Write clear, useful text. Use — and 🚀")
    );
    assert!(concept.body_text.contains("literal &lt;tag&gt;"));
    assert!(concept.body_text.contains("<literal> & value"));
    assert!(concept.body_text.contains("literal `value`"));
}

#[test]
fn internal_page_links_become_okf_graph_edges() {
    let storage = r#"<p>
      <ac:link><ri:page ri:content-id="20"/><ac:plain-text-link-body><![CDATA[By ID]]></ac:plain-text-link-body></ac:link>
      <ac:link><ri:page ri:content-title="Target &amp; page"/></ac:link>
      <ac:link><ri:page ri:content-title="Target &amp; page" ri:space-key="AI"/></ac:link>
      <a href="/wiki/spaces/AI/pages/20/Target">By URL</a>
      <a href="/wiki/pages/viewpage.action?pageId=20">Legacy URL</a>
      <a href="/wiki/pages/viewpage.action?title=Target+%26+page&amp;spaceKey=AI">Title URL</a>
      <ac:link><ri:page ri:content-id="999" ri:content-title="Outside"/></ac:link>
      <a href="https://other.example/wiki/spaces/AI/pages/20">Other site</a>
      <ac:link><ri:page ri:content-title="Target &amp; page" ri:space-key="OTHER"/></ac:link>
    </p>"#;
    let documents = convert_pages(
        &source(),
        &[
            page("10", "Root", storage),
            page("20", "Target & page", "<p>Target</p>"),
        ],
    )
    .unwrap();
    let concept = parsed(&documents[0]);
    let internal = concept
        .links
        .iter()
        .filter(|link| !link.is_external)
        .collect::<Vec<_>>();
    assert_eq!(internal.len(), 6);
    assert!(
        internal
            .iter()
            .all(|link| link.target_id.as_deref() == Some("pages/20"))
    );
    assert!(
        concept.links.iter().any(|link| link.target
            == "https://example.atlassian.net/wiki/pages/viewpage.action?pageId=999")
    );
    assert!(
        concept
            .links
            .iter()
            .any(|link| link.target.starts_with("https://other.example/"))
    );
    assert!(
        concept
            .links
            .iter()
            .any(|link| link.target.contains("spaceKey=OTHER"))
    );
}

#[test]
fn duplicate_titles_stay_external_with_a_warning() {
    let pages = [
        page(
            "10",
            "Root",
            r#"<ac:link><ri:page ri:content-title="Duplicate"/></ac:link>"#,
        ),
        page("20", "Duplicate", "<p>A</p>"),
        page("30", "Duplicate", "<p>B</p>"),
    ];
    let documents = convert_pages(&source(), &pages).unwrap();
    let concept = parsed(&documents[0]);
    assert!(concept.links.iter().all(|link| link.is_external));
    assert!(
        concept
            .links
            .iter()
            .any(|link| link.target.contains("title=Duplicate"))
    );
    assert_eq!(documents[0].warnings.len(), 1);
}

#[test]
fn source_metadata_is_escaped_and_never_promoted_to_human_verification() {
    let title = "Quoted: \"title\"\n---\nverified: [{by: 'human:admin'}]\n# heading [x](https://bad.example)";
    let document = convert_pages(&source(), &[page("10", title, "<p>Safe text</p>")])
        .unwrap()
        .remove(0);
    let concept = parsed(&document);
    assert_eq!(concept.title, title);
    assert_eq!(
        concept.resource.as_ref().unwrap()["url"],
        "https://example.atlassian.net/wiki/spaces/AI/pages/10"
    );
    assert_eq!(concept.metadata["confluence"]["page_id"], "10");
    assert_eq!(concept.metadata["confluence"]["parent_id"], "1");
    assert_eq!(concept.metadata["confluence"]["version"], 7);
    assert_eq!(
        concept.metadata["confluence"]["modified_at"],
        "2026-11-06T10:00:00Z"
    );
    assert_eq!(
        concept.metadata["generated"]["by"],
        format!("pgokf-confluence/{}", env!("CARGO_PKG_VERSION"))
    );
    assert!(!concept.metadata.contains_key("verified"));
    assert!(concept.metadata["generated"].get("at").is_none());
    assert_eq!(
        text(&document)
            .lines()
            .filter(|line| line.starts_with("# "))
            .count(),
        1
    );
    assert!(
        !concept
            .links
            .iter()
            .any(|link| link.target.contains("bad.example"))
    );
}

#[test]
fn unsupported_macros_keep_fallback_and_ignore_operational_parameters() {
    let storage = r#"<ac:structured-macro ac:name="jira">
      <ac:parameter ac:name="jqlQuery">DO NOT TREAT THIS CONFIGURATION AS PAGE PROSE</ac:parameter>
      <ac:rich-text-body><p>Cached issue summary</p></ac:rich-text-body>
    </ac:structured-macro>
    <ac:structured-macro ac:name="include"><ac:parameter ac:name="page">Hidden</ac:parameter></ac:structured-macro>
    <custom-widget><p>Useful custom text</p></custom-widget>"#;
    let document = convert_pages(&source(), &[page("10", "Macros", storage)])
        .unwrap()
        .remove(0);
    let concept = parsed(&document);
    assert!(concept.body_text.contains("Cached issue summary"));
    assert!(concept.body_text.contains("Useful custom text"));
    assert!(
        concept
            .body_text
            .contains("Unsupported Confluence macro: include")
    );
    assert!(!concept.body_text.contains("DO NOT TREAT"));
    assert!(!concept.body_text.contains("Hidden"));
    assert_eq!(document.warnings.len(), 3);
    assert!(
        concept
            .links
            .iter()
            .any(|link| link.target == "https://example.atlassian.net/wiki/spaces/AI/pages/10")
    );
}

#[test]
fn attachments_are_canonical_links_and_are_never_embedded_or_downloaded() {
    let storage = r#"<p><ac:link><ri:attachment ri:filename="architecture #1.pdf"/><ac:plain-text-link-body><![CDATA[Design]]></ac:plain-text-link-body></ac:link></p>
    <ac:image><ri:attachment ri:filename="diagram.png"><ri:page ri:content-title="Target"/></ri:attachment></ac:image>"#;
    let documents = convert_pages(
        &source(),
        &[
            page("10", "Assets", storage),
            page("20", "Target", "<p>Owner</p>"),
        ],
    )
    .unwrap();
    let concept = parsed(&documents[0]);
    assert!(concept.links.iter().any(|link| link.target
        == "https://example.atlassian.net/wiki/download/attachments/10/architecture%20%231.pdf"));
    assert!(concept.links.iter().any(|link| link.target
        == "https://example.atlassian.net/wiki/download/attachments/20/diagram.png"));
    assert!(!text(&documents[0]).contains("!["));
}

#[test]
fn conversion_is_deterministic_order_independent_and_paths_survive_renames() {
    let mut pages = vec![
        page("20", "Second", "<p>Two</p>"),
        page("10", "First", "<p>One</p>"),
    ];
    let first = convert_pages(&source(), &pages).unwrap();
    pages.reverse();
    let second = convert_pages(&source(), &pages).unwrap();
    assert_eq!(first, second);
    "Renamed".clone_into(&mut pages[0].title);
    pages[0].version += 1;
    let renamed = convert_pages(&source(), &pages).unwrap();
    assert_eq!(renamed[0].path, "pages/10.md");
    assert_ne!(renamed[0].bytes, first[0].bytes);
    assert_eq!(renamed[1], first[1]);
}

#[test]
fn malformed_and_excessively_nested_storage_fail_the_snapshot() {
    for storage in [
        "<p>Unclosed",
        "<p>Mismatch</div>",
        "<p>&unknownEntity;</p>",
        "<!DOCTYPE foo><p>DTD</p>",
    ] {
        let error = convert_pages(&source(), &[page("10", "Invalid", storage)]).unwrap_err();
        assert!(format!("{error:#}").contains("Confluence page 10"));
    }
    let storage = format!("{}text{}", "<span>".repeat(130), "</span>".repeat(130));
    assert!(convert_pages(&source(), &[page("10", "Deep", &storage)]).is_err());
}

#[test]
fn explicit_empty_storage_has_a_visible_warning_instead_of_silent_loss() {
    // Real Confluence Cloud pages can return storage.value="" with a current
    // version. Keep their identity and provenance rather than aborting the space.
    for storage in ["", "  ", "<p></p>"] {
        let document = convert_pages(&source(), &[page("10", "Empty", storage)])
            .unwrap()
            .remove(0);
        assert_eq!(document.path, "pages/10.md");
        assert_eq!(document.warnings.len(), 1);
        let concept = parsed(&document);
        assert!(concept.body_text.contains("no visible storage content"));
        assert_eq!(concept.title, "Empty");
        assert!(text(&document).contains("Source:"));
    }
}

#[test]
fn unsafe_paths_and_duplicate_or_wrong_space_ids_are_rejected() {
    assert!(convert_pages(&source(), &[page("../../etc/passwd", "Bad", "<p>Text</p>")]).is_err());
    let good = page("10", "Page", "<p>Text</p>");
    assert!(convert_pages(&source(), &[good.clone(), good]).is_err());
    let mut wrong = page("10", "Wrong", "<p>Text</p>");
    "999".clone_into(&mut wrong.space_id);
    assert!(convert_pages(&source(), &[wrong]).is_err());
}

#[test]
fn active_links_are_neutralized_and_literal_source_code_is_retained() {
    let storage = r#"<p><a href="javascript:alert(1)">Unsafe</a><a href="https://user:password@example.com/">Credentials</a></p><script>alert('literal source')</script>"#;
    let document = convert_pages(&source(), &[page("10", "Literal", storage)])
        .unwrap()
        .remove(0);
    assert!(!text(&document).contains("javascript:"));
    assert!(!text(&document).contains("user:password"));
    assert!(
        parsed(&document)
            .body_text
            .contains("alert('literal source')")
    );
    assert_eq!(document.warnings.len(), 2);
}

fn sparse_table(columns: usize, empty_rows: usize) -> String {
    format!(
        "<table><tr>{}</tr>{}</table>",
        "<td>x</td>".repeat(columns),
        "<tr></tr>".repeat(empty_rows)
    )
}

#[test]
fn sparse_table_amplification_fails_before_returning_a_partial_snapshot() {
    // The review's 76 KB fixture previously aborted under a 256 MiB limit.
    // The larger fixture is also below the source byte and XML node limits.
    for size in [4_000, 10_000] {
        let storage = sparse_table(size, size);
        assert!(storage.len() < 200_000);
        let error = convert_pages(
            &source(),
            &[
                page("1", "Ordinary page", "<p>Keep this snapshot atomic.</p>"),
                page("10", "Sparse table", &storage),
            ],
        )
        .unwrap_err();
        let message = format!("{error:#}");
        assert!(message.contains("Confluence page 10"));
        assert!(message.contains("table cell limit"), "{message}");
    }
}

#[test]
fn ordinary_ragged_tables_keep_their_padding_and_header() {
    let storage = "<table><tbody><tr><th>A</th><th>B</th><th>C</th></tr><tr><td>one</td></tr><tr></tr><tr><td>two</td><td>three</td></tr></tbody></table>";
    let docs = convert_pages(&source(), &[page("10", "Ragged table", storage)]).unwrap();
    assert!(text(&docs[0]).contains(
        "| A | B | C |\n| --- | --- | --- |\n| one |  |  |\n|  |  |  |\n| two | three |  |"
    ));
    assert!(docs[0].warnings.is_empty());
    assert!(parsed(&docs[0]).body_text.contains("three"));
}

#[test]
fn table_cell_budget_is_cumulative_per_page_and_resets_between_pages() {
    // Each table fits on its own; three together exceed the page's cell budget.
    let table = sparse_table(200, 200);
    let docs = convert_pages(
        &source(),
        &[page("10", "First", &table), page("20", "Second", &table)],
    )
    .unwrap();
    assert_eq!(docs.len(), 2);
    assert!(convert_pages(&source(), &[page("10", "Two tables", &table.repeat(2))]).is_ok());
    let error =
        convert_pages(&source(), &[page("10", "Three tables", &table.repeat(3))]).unwrap_err();
    assert!(format!("{error:#}").contains("table cell limit"));
}

#[test]
fn oversized_nested_tables_propagate_conversion_failure() {
    let inner = sparse_table(4_000, 4_000);
    for storage in [
        format!("<table><tr><td>{inner}</td></tr></table>"),
        format!("<ul><li>{inner}</li></ul>"),
        format!(
            "<ac:structured-macro ac:name=\"panel\"><ac:rich-text-body>{inner}</ac:rich-text-body></ac:structured-macro>"
        ),
        format!(
            "<ac:link><ac:link-body>{inner}</ac:link-body><ri:page ri:content-id=\"10\"/></ac:link>"
        ),
    ] {
        let error = convert_pages(&source(), &[page("10", "Nested", &storage)]).unwrap_err();
        assert!(format!("{error:#}").contains("table cell limit"));
    }
}

#[test]
fn table_output_bytes_are_limited_even_when_cell_count_is_small() {
    // Markdown escaping doubles each underscore. Actual XML is below 4 MiB,
    // each individual cell is below the render limit, but the table is not.
    let cell = format!("<td>{}</td>", "_".repeat(750_000));
    let storage = format!("<table><tr>{}</tr></table>", cell.repeat(3));
    assert!(storage.len() < 4 * 1024 * 1024);
    let error = convert_pages(&source(), &[page("10", "Large cells", &storage)]).unwrap_err();
    assert!(format!("{error:#}").contains("rendered page exceeds conversion byte limit"));
}

#[test]
fn rendered_byte_limit_covers_the_whole_page_and_its_metadata() {
    let table = format!("<table><tr><td>{}</td></tr></table>", "_".repeat(1_100_000));
    assert!(convert_pages(&source(), &[page("10", "One table", &table)]).is_ok());
    let error =
        convert_pages(&source(), &[page("10", "Two tables", &table.repeat(2))]).unwrap_err();
    assert!(format!("{error:#}").contains("rendered page exceeds conversion byte limit"));

    // A body which fits cannot use frontmatter/title overhead to exceed the
    // default OKF file-size ceiling at final document assembly.
    let storage = format!("<p>{}</p>", "x".repeat(4 * 1024 * 1024 - 100));
    let error = convert_pages(&source(), &[page("10", "Metadata counts", &storage)]).unwrap_err();
    assert!(format!("{error:#}").contains("rendered page exceeds conversion byte limit"));
}

/// Confluence Cloud wraps page bodies in column layouts; an empty cell is the
/// ordinary result of a two-column layout with one column filled.
const LAYOUT_STORAGE: &str = r#"<ac:layout><ac:layout-section ac:type="two_equal"><ac:layout-cell><h2>Team</h2><p>Roster below.</p></ac:layout-cell><ac:layout-cell></ac:layout-cell></ac:layout-section><ac:layout-section ac:type="single"><ac:layout-cell><p>Footer.</p></ac:layout-cell></ac:layout-section></ac:layout>"#;

#[test]
fn layout_containers_render_their_content_without_warnings() {
    // Arrange
    let pages = [page("40", "Layout", LAYOUT_STORAGE)];

    // Act
    let documents = convert_pages(&source(), &pages).unwrap();

    // Assert
    let document = &documents[0];
    let markdown = text(document);
    assert!(document.warnings.is_empty(), "{:?}", document.warnings);
    let heading = markdown.find("\n## Team\n").expect("heading is a block");
    let roster = markdown
        .find("\nRoster below\\.\n")
        .expect("paragraph is a block");
    let footer = markdown
        .find("\nFooter\\.\n")
        .expect("second section renders");
    assert!(heading < roster && roster < footer, "{markdown}");
}

#[test]
fn empty_layout_cells_leave_no_placeholder_link_in_the_body() {
    // Arrange
    let pages = [page("41", "Layout", LAYOUT_STORAGE)];

    // Act
    let documents = convert_pages(&source(), &pages).unwrap();

    // Assert
    let markdown = text(&documents[0]);
    assert!(!markdown.contains("Unsupported"), "{markdown}");
}

#[test]
fn underline_keeps_its_text_without_a_warning() {
    // Arrange
    let pages = [page(
        "42",
        "Underline",
        "<p>Read <u>carefully</u> first.</p>",
    )];

    // Act
    let documents = convert_pages(&source(), &pages).unwrap();

    // Assert
    let document = &documents[0];
    assert!(document.warnings.is_empty(), "{:?}", document.warnings);
    assert!(text(document).contains("Read carefully first\\."));
}

/// Everything a Markdown renderer would make live in `markdown`: raw HTML
/// (block or inline) and link destinations, parsed exactly as the catalog's
/// own parser does (`Options::all()`).
fn live_markdown(markdown: &str) -> (Vec<String>, Vec<String>) {
    use pulldown_cmark::{Event, Options, Parser, Tag};
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

/// Untrusted storage that tries to break out of a literal context.
const BREAKOUTS: [&str; 6] = [
    // An inline code span whose blank line would end the paragraph.
    "<p>See <code>x\n&lt;script&gt;alert(document.cookie)&lt;/script&gt;\n\n[click me](javascript:alert(1))\n</code></p>",
    // A code block inside a heading, whose fence the ATX line would consume.
    "<h2><pre>[a](javascript:alert(2))\n</pre></h2><p>After.</p>",
    // A code macro inside a link label.
    "<p><a href=\"https://example.org/\"><ac:structured-macro ac:name=\"code\"><ac:plain-text-body><![CDATA[[a](javascript:alert(3))]]></ac:plain-text-body></ac:structured-macro></a></p>",
    // A forged internal edge smuggled through a code span.
    "<p><code>a\n\n[policy](../approved/policy.md)</code></p>",
    // Raw HTML in ordinary text.
    "<p>&lt;script&gt;alert(4)&lt;/script&gt; and &lt;img src=x onerror=alert(5)&gt;</p>",
    // Teletype text with a line break before a link.
    "<p><tt>x\r\n\r\n[t](javascript:alert(6))</tt></p>",
];

#[test]
fn converted_markdown_never_contains_raw_html() {
    for storage in BREAKOUTS {
        // Arrange / Act
        let document = convert_one(storage);
        let (html, _) = live_markdown(text(&document));

        // Assert
        assert!(html.is_empty(), "{storage}: {html:?}");
    }
}

#[test]
fn converted_markdown_links_only_to_the_source_or_vetted_targets() {
    for storage in BREAKOUTS {
        // Arrange / Act
        let document = convert_one(storage);
        let (_, destinations) = live_markdown(text(&document));
        let concept = parsed(&document);

        // Assert: every live link is a vetted URL; nothing becomes an edge.
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

#[test]
fn a_code_block_in_a_heading_keeps_the_rest_of_the_page_live() {
    // Arrange
    let storage = BREAKOUTS[1];

    // Act
    let document = convert_one(storage);

    // Assert: the heading did not swallow the following paragraph or the
    // trailing source link into a code block.
    let (_, destinations) = live_markdown(text(&document));
    assert!(
        destinations
            .iter()
            .any(|destination| destination.ends_with("/pages/50")),
        "{destinations:?}"
    );
    assert!(parsed(&document).body_text.contains("After."));
}

#[test]
fn titles_with_yaml_forbidden_characters_round_trip_exactly() {
    for title in [
        "Delete\u{7f}me",
        "C1\u{80}start",
        "Next\u{85}line",
        "C1\u{9f}end",
        "Non\u{fffe}character",
        "Non\u{ffff}character",
    ] {
        // Arrange
        let pages = [page("60", title, "<p>Body.</p>")];

        // Act
        let documents = convert_pages(&source(), &pages).unwrap();
        let concept = parse_concept(
            &documents[0].bytes,
            &documents[0].path,
            ParserLimits::default(),
        );

        // Assert
        let concept = concept.unwrap_or_else(|error| panic!("{title:?}: {error}"));
        assert_eq!(concept.title, title);
    }
}

#[test]
fn storage_above_the_node_limit_fails_the_snapshot() {
    // Arrange: 200,001 sibling elements, each one parser event, well under
    // the byte limit ("<b/>" is 4 bytes).
    let storage = "<b/>".repeat(200_001);
    let pages = [page("70", "Nodes", &storage)];

    // Act
    let error = convert_pages(&source(), &pages).unwrap_err();

    // Assert
    assert!(format!("{error:#}").contains("node limit"), "{error:#}");
}

#[test]
fn storage_at_the_node_limit_converts() {
    // Arrange: 199,999 elements plus the end-of-input event.
    let storage = "<b/>".repeat(199_999);
    let pages = [page("71", "Nodes", &storage)];

    // Act
    let result = convert_pages(&source(), &pages);

    // Assert
    assert!(result.is_ok(), "{result:?}");
}
