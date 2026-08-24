use std::path::Path;

use okf_parser::{Error, LinkKind, ParserLimits, normalize_path, parse_concept};

const RICH: &[u8] = include_bytes!("fixtures/rich.md");

#[test]
fn parses_known_fields_metadata_body_and_links() {
    let parsed = parse_concept(RICH, "concepts\\failover.MD", ParserLimits::default()).unwrap();

    assert_eq!(parsed.id, "incident-db");
    assert_eq!(parsed.path, "concepts/failover.md");
    assert_eq!(parsed.r#type, "Runbook");
    assert_eq!(parsed.title, "Database failover");
    assert_eq!(parsed.description.as_deref(), Some("Recover the primary safely"));
    assert_eq!(parsed.tags, ["postgres", "incident"]);
    assert_eq!(parsed.resource.unwrap()["url"], "https://example.test/runbooks/db");
    assert_eq!(parsed.metadata["owner"], "sre");
    assert_eq!(parsed.metadata["severity"], "high");
    assert_eq!(
        parsed.body_text,
        "Database failover\nFollow the replication checklist.\nSee replica health and https://status.example.test.\ntopology"
    );
    assert_eq!(parsed.links.len(), 3);
    assert_eq!(parsed.links[0].target, "replica.md");
    assert_eq!(parsed.links[0].label, "replica health");
    assert_eq!(parsed.links[0].kind, LinkKind::Inline);
    assert_eq!(parsed.links[0].ordinal, 0);
    assert_eq!(parsed.links[1].kind, LinkKind::Autolink);
    assert_eq!(parsed.links[2].kind, LinkKind::Image);
    assert_eq!(parsed.links[2].label, "topology");
}

#[test]
fn uses_normalized_path_as_fallback_id() {
    let source = b"---\ntype: Note\ntitle: Hello\n---\nBody";
    let parsed = parse_concept(source, "notes/hello", ParserLimits::default()).unwrap();
    assert_eq!(parsed.id, "notes/hello.md");
}

#[test]
fn supports_crlf_delimiters_and_unicode() {
    let source = "---\r\ntype: Note\r\ntitle: Café ☕\r\n---\r\n# Héllo\r\n";
    let parsed = parse_concept(source.as_bytes(), "知识/café.md", ParserLimits::default()).unwrap();
    assert_eq!(parsed.title, "Café ☕");
    assert_eq!(parsed.body_text, "Héllo");
}

#[test]
fn rejects_missing_unterminated_and_malformed_frontmatter() {
    assert!(matches!(
        parse_concept(b"# no yaml", "x.md", ParserLimits::default()),
        Err(Error::MissingFrontmatter)
    ));
    assert!(matches!(
        parse_concept(b"---\ntype: Note\ntitle: no end", "x.md", ParserLimits::default()),
        Err(Error::UnterminatedFrontmatter)
    ));
    assert!(matches!(
        parse_concept(
            include_bytes!("fixtures/malformed.md"),
            "x.md",
            ParserLimits::default()
        ),
        Err(Error::InvalidFrontmatter(_))
    ));
}

#[test]
fn enforces_file_and_frontmatter_limits() {
    let limits = ParserLimits {
        max_file_bytes: 3,
        max_frontmatter_bytes: 100,
    };
    assert!(matches!(
        parse_concept(b"four", "x.md", limits),
        Err(Error::FileTooLarge { actual: 4, limit: 3 })
    ));

    let limits = ParserLimits {
        max_file_bytes: 100,
        max_frontmatter_bytes: 4,
    };
    assert!(matches!(
        parse_concept(b"---\ntype: Note\ntitle: X\n---\n", "x.md", limits),
        Err(Error::FrontmatterTooLarge { .. })
    ));
}

#[test]
fn rejects_unsafe_or_non_markdown_paths() {
    assert!(matches!(normalize_path(Path::new("../secret.md")), Err(Error::PathTraversal(_))));
    assert!(matches!(normalize_path(Path::new("/tmp/x.md")), Err(Error::AbsolutePath(_))));
    assert!(matches!(normalize_path(Path::new("C:\\tmp\\x.md")), Err(Error::AbsolutePath(_))));
    assert!(matches!(normalize_path(Path::new("x.txt")), Err(Error::UnsupportedExtension(_))));
    assert_eq!(normalize_path(Path::new("./a//b")).unwrap(), "a/b.md");
}

#[test]
fn extracts_reference_email_and_formatted_labels() {
    let source = b"---\ntype: Note\ntitle: Links\n---\n[**bold** `code`][ref] <a@example.com>\n\n[ref]: target.md";
    let parsed = parse_concept(source, "links.md", ParserLimits::default()).unwrap();
    assert_eq!(parsed.links[0].label, "bold code");
    assert_eq!(parsed.links[0].kind, LinkKind::Reference);
    assert_eq!(parsed.links[1].kind, LinkKind::Email);
}

#[test]
fn rejects_invalid_utf8() {
    assert!(matches!(
        parse_concept(&[0xff], "x.md", ParserLimits::default()),
        Err(Error::InvalidUtf8(_))
    ));
}

#[test]
fn preserves_nested_image_and_outer_link_in_source_order() {
    let source = b"---\ntype: Note\ntitle: Nested\n---\n[![diagram](diagram.png)](details.md)";
    let parsed = parse_concept(source, "nested.md", ParserLimits::default()).unwrap();
    assert_eq!(parsed.links.len(), 2);
    assert_eq!(parsed.links[0].target, "details.md");
    assert_eq!(parsed.links[0].kind, LinkKind::Inline);
    assert_eq!(parsed.links[0].label, "diagram");
    assert_eq!(parsed.links[1].target, "diagram.png");
    assert_eq!(parsed.links[1].kind, LinkKind::Image);
}
