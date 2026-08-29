// SPDX-License-Identifier: AGPL-3.0-only
use std::path::Path;

use okf_parser::{Error, ErrorCategory, LinkKind, ParserLimits, normalize_path, parse_concept};

const RICH: &[u8] = include_bytes!("fixtures/rich.md");

#[test]
fn parses_known_fields_metadata_body_and_links() {
    let parsed = parse_concept(RICH, "concepts/failover.MD", ParserLimits::default()).unwrap();

    assert_eq!(parsed.id, "concepts/failover");
    assert_eq!(parsed.declared_id.as_deref(), Some("incident-db"));
    assert_eq!(parsed.path, "concepts/failover.md");
    assert_eq!(parsed.r#type, "Runbook");
    assert_eq!(parsed.title, "Database failover");
    assert_eq!(
        parsed.description.as_deref(),
        Some("Recover the primary safely")
    );
    assert_eq!(parsed.tags, ["postgres", "incident"]);
    assert_eq!(
        parsed.resource.unwrap()["url"],
        "https://example.test/runbooks/db"
    );
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
    assert!(!parsed.links[0].is_external);
    assert_eq!(
        parsed.links[0].target_path.as_deref(),
        Some("concepts/replica.md")
    );
    assert_eq!(
        parsed.links[0].target_id.as_deref(),
        Some("concepts/replica")
    );
    assert_eq!(parsed.links[1].kind, LinkKind::Autolink);
    assert!(parsed.links[1].is_external);
    assert_eq!(parsed.links[1].target_path, None);
    assert_eq!(parsed.links[1].target_id, None);
    assert_eq!(parsed.links[2].kind, LinkKind::Image);
    assert_eq!(parsed.links[2].label, "topology");
    assert!(!parsed.links[2].is_external);
    assert_eq!(parsed.links[2].target_path, None);
}

#[test]
fn derives_id_from_normalized_path_without_md_suffix() {
    let source = b"---\ntype: Note\ntitle: Hello\n---\nBody";

    let parsed = parse_concept(source, "notes/hello", ParserLimits::default()).unwrap();

    assert_eq!(parsed.id, "notes/hello");
    assert_eq!(parsed.path, "notes/hello.md");
    assert_eq!(parsed.declared_id, None);
}

#[test]
fn never_trusts_declared_id_over_path_derived_id() {
    let source = b"---\nid: producer-supplied\ntype: Note\ntitle: Hello\n---\nBody";

    let parsed = parse_concept(source, "notes/hello.md", ParserLimits::default()).unwrap();

    assert_eq!(parsed.id, "notes/hello");
    assert_eq!(parsed.declared_id.as_deref(), Some("producer-supplied"));
}

#[test]
fn rejects_reserved_index_and_log_files() {
    let source = b"---\ntype: Note\ntitle: Reserved\n---\nBody";

    for reserved in ["index.md", "log.md", "nested/index.md", "deep/dir/log.md"] {
        let error = parse_concept(source, reserved, ParserLimits::default()).unwrap_err();
        assert!(
            matches!(&error, Error::ReservedPath { path } if path == reserved),
            "expected ReservedPath for {reserved}, got {error:?}"
        );
        assert_eq!(error.category(), ErrorCategory::Reserved);
    }

    let not_reserved = parse_concept(source, "reindex.md", ParserLimits::default());
    assert!(not_reserved.is_ok());
}

#[test]
fn supports_crlf_delimiters_and_unicode() {
    let source = "---\r\ntype: Note\r\ntitle: Café ☕\r\n---\r\n# Héllo\r\n";
    let parsed = parse_concept(source.as_bytes(), "知识/café.md", ParserLimits::default()).unwrap();
    assert_eq!(parsed.title, "Café ☕");
    assert_eq!(parsed.body_text, "Héllo");
    assert_eq!(parsed.id, "知识/café");
}

#[test]
fn rejects_missing_unterminated_and_malformed_frontmatter() {
    assert!(matches!(
        parse_concept(b"# no yaml", "x.md", ParserLimits::default()),
        Err(Error::MissingFrontmatter { .. })
    ));
    assert!(matches!(
        parse_concept(
            b"---\ntype: Note\ntitle: no end",
            "x.md",
            ParserLimits::default()
        ),
        Err(Error::UnterminatedFrontmatter { .. })
    ));
    assert!(matches!(
        parse_concept(
            include_bytes!("fixtures/malformed.md"),
            "x.md",
            ParserLimits::default()
        ),
        Err(Error::InvalidFrontmatter { .. })
    ));
}

#[test]
fn rejects_metadata_that_cannot_become_json() {
    let source = b"---\ntype: Note\ntitle: Meta\ncustom:\n  ? [a, b]\n  : sequence-key\n---\nBody";

    let error = parse_concept(source, "notes/meta.md", ParserLimits::default()).unwrap_err();

    assert!(matches!(&error, Error::InvalidMetadata { path, .. } if path == "notes/meta.md"));
    assert_eq!(error.category(), ErrorCategory::Metadata);
}

#[test]
fn enforces_file_and_frontmatter_limits() {
    let limits = ParserLimits {
        max_file_bytes: 3,
        max_frontmatter_bytes: 100,
    };
    assert!(matches!(
        parse_concept(b"four", "x.md", limits),
        Err(Error::FileTooLarge {
            actual: 4,
            limit: 3,
            ..
        })
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
fn rejects_unsafe_empty_or_non_markdown_paths() {
    assert!(matches!(
        normalize_path(Path::new("../secret.md")),
        Err(Error::PathTraversal { .. })
    ));
    assert!(matches!(
        normalize_path(Path::new("/tmp/x.md")),
        Err(Error::AbsolutePath { .. })
    ));
    assert!(matches!(
        normalize_path(Path::new("x.txt")),
        Err(Error::UnsupportedExtension { .. })
    ));
    assert!(matches!(
        normalize_path(Path::new("")),
        Err(Error::EmptyPath)
    ));
    assert_eq!(normalize_path(Path::new("./a//b")).unwrap(), "a/b.md");
}

#[cfg(unix)]
#[test]
fn preserves_posix_filenames_containing_backslashes() {
    // On POSIX `\` is a legal file-name byte, not a separator: the normalized
    // path must keep it verbatim so it still names the file on disk.
    assert_eq!(
        normalize_path(Path::new(r"back\slash.md")).unwrap(),
        r"back\slash.md"
    );
    assert_eq!(
        normalize_path(Path::new(r"notes/back\slash.MD")).unwrap(),
        r"notes/back\slash.md"
    );
}

#[cfg(windows)]
#[test]
fn folds_windows_separators_and_rejects_drive_absolute_paths() {
    assert_eq!(
        normalize_path(Path::new("concepts\\failover.MD")).unwrap(),
        "concepts/failover.md"
    );
    assert!(matches!(
        normalize_path(Path::new("C:\\tmp\\x.md")),
        Err(Error::AbsolutePath { .. })
    ));
}

#[cfg(unix)]
#[test]
fn rejects_non_utf8_paths() {
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;
    let non_utf8 = OsStr::from_bytes(b"notes/bad-\xff.md");

    let error = normalize_path(Path::new(non_utf8)).unwrap_err();

    assert!(matches!(&error, Error::NonUtf8Path { path } if path.contains("bad-")));
    assert_eq!(error.category(), ErrorCategory::Path);
}

#[test]
fn errors_carry_offending_path_and_category() {
    let missing = parse_concept(b"# no yaml", "notes/x.md", ParserLimits::default()).unwrap_err();
    assert_eq!(missing.path(), "notes/x.md");
    assert_eq!(missing.category(), ErrorCategory::Frontmatter);

    let limits = ParserLimits {
        max_file_bytes: 1,
        max_frontmatter_bytes: 100,
    };
    let too_large = parse_concept(b"four", "notes/big.MD", limits).unwrap_err();
    assert_eq!(too_large.path(), "notes/big.md");
    assert_eq!(too_large.category(), ErrorCategory::Limit);

    let non_utf8 = parse_concept(&[0xff], "notes/enc.md", ParserLimits::default()).unwrap_err();
    assert_eq!(non_utf8.path(), "notes/enc.md");
    assert_eq!(non_utf8.category(), ErrorCategory::Encoding);

    let traversal = normalize_path(Path::new("../secret.md")).unwrap_err();
    assert_eq!(traversal.path(), "../secret.md");
    assert_eq!(traversal.category(), ErrorCategory::Path);

    let empty = normalize_path(Path::new("")).unwrap_err();
    assert_eq!(empty.path(), "");
    assert_eq!(empty.category(), ErrorCategory::Path);
}

#[test]
fn extracts_reference_email_and_formatted_labels() {
    let source = b"---\ntype: Note\ntitle: Links\n---\n[**bold** `code`][ref] <a@example.com>\n\n[ref]: target.md";
    let parsed = parse_concept(source, "links.md", ParserLimits::default()).unwrap();
    assert_eq!(parsed.links[0].label, "bold code");
    assert_eq!(parsed.links[0].kind, LinkKind::Reference);
    assert_eq!(parsed.links[0].target_path.as_deref(), Some("target.md"));
    assert_eq!(parsed.links[1].kind, LinkKind::Email);
    assert!(parsed.links[1].is_external);
    assert_eq!(parsed.links[1].target_path, None);
}

#[test]
fn normalizes_internal_link_targets_relative_to_source_file() {
    let source = b"---\ntype: Note\ntitle: Links\n---\n\
[up](../services/db.md) [root](/runbooks/failover.md) [frag](other.md#section) \
[self](#top) [bare](sibling) [escape](../../outside.md) [proto](//cdn.example.test/x.js)";

    let parsed = parse_concept(source, "dashboards/health.md", ParserLimits::default()).unwrap();

    let by_target = |target: &str| {
        parsed
            .links
            .iter()
            .find(|link| link.target == target)
            .unwrap_or_else(|| panic!("missing link {target}"))
    };

    let up = by_target("../services/db.md");
    assert_eq!(up.target_path.as_deref(), Some("services/db.md"));
    assert_eq!(up.target_id.as_deref(), Some("services/db"));

    let root = by_target("/runbooks/failover.md");
    assert_eq!(root.target_path.as_deref(), Some("runbooks/failover.md"));
    assert_eq!(root.target_id.as_deref(), Some("runbooks/failover"));

    let fragment = by_target("other.md#section");
    assert_eq!(fragment.target_path.as_deref(), Some("dashboards/other.md"));
    assert_eq!(fragment.target_id.as_deref(), Some("dashboards/other"));

    let self_link = by_target("#top");
    assert_eq!(
        self_link.target_path.as_deref(),
        Some("dashboards/health.md")
    );
    assert_eq!(self_link.target_id.as_deref(), Some("dashboards/health"));

    let bare = by_target("sibling");
    assert_eq!(bare.target_path.as_deref(), Some("dashboards/sibling.md"));

    let escape = by_target("../../outside.md");
    assert!(!escape.is_external);
    assert_eq!(escape.target_path, None);
    assert_eq!(escape.target_id, None);

    let protocol_relative = by_target("//cdn.example.test/x.js");
    assert!(protocol_relative.is_external);
    assert_eq!(protocol_relative.target_path, None);
}

#[test]
fn rejects_invalid_utf8() {
    assert!(matches!(
        parse_concept(&[0xff], "x.md", ParserLimits::default()),
        Err(Error::InvalidUtf8 { .. })
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
    assert_eq!(parsed.links[0].target_path.as_deref(), Some("details.md"));
    assert_eq!(parsed.links[1].target, "diagram.png");
    assert_eq!(parsed.links[1].kind, LinkKind::Image);
    assert_eq!(parsed.links[1].target_path, None);
}

#[test]
fn empty_inline_link_destination_produces_no_edge() {
    // Regression (F12): `[label]()` has a genuinely empty destination. The
    // link is still recorded (with an empty raw target), but it must resolve to
    // no target path or id rather than fabricating a self-referential edge.
    let source = b"---\ntype: Note\ntitle: Empty\n---\nSee [label]() here.";

    let parsed = parse_concept(source, "notes/a.md", ParserLimits::default()).unwrap();

    let empty = parsed
        .links
        .iter()
        .find(|link| link.target.is_empty())
        .expect("empty inline destination should still be recorded as a link");
    assert!(!empty.is_external);
    assert_eq!(empty.target_path, None);
    assert_eq!(empty.target_id, None);
}

#[test]
fn fragment_only_link_self_references_but_named_target_does_not() {
    // Regression (F12): a fragment-only `[label](#frag)` resolves to the source
    // concept, while `[label](other.md)` resolves to a sibling — neither path
    // should collapse the other's behavior.
    let source = b"---\ntype: Note\ntitle: Links\n---\n[here](#frag) and [there](other.md)";

    let parsed = parse_concept(source, "notes/a.md", ParserLimits::default()).unwrap();

    let by_target = |target: &str| {
        parsed
            .links
            .iter()
            .find(|link| link.target == target)
            .unwrap_or_else(|| panic!("missing link {target}"))
    };

    let fragment = by_target("#frag");
    assert_eq!(fragment.target_path.as_deref(), Some("notes/a.md"));
    assert_eq!(fragment.target_id.as_deref(), Some("notes/a"));

    let named = by_target("other.md");
    assert_eq!(named.target_path.as_deref(), Some("notes/other.md"));
    assert_eq!(named.target_id.as_deref(), Some("notes/other"));
}

#[test]
fn leading_utf8_bom_parses_identically_to_bom_free_input() {
    // Regression (F13): a leading U+FEFF BOM before the `---` delimiter must be
    // stripped so the file parses instead of being rejected as
    // MissingFrontmatter, and it must yield the same concept as the BOM-free
    // input.
    let plain = "---\ntype: Note\ntitle: Bommed\n---\nBody \u{feff}mid".as_bytes();
    let bommed = "\u{feff}---\ntype: Note\ntitle: Bommed\n---\nBody \u{feff}mid".as_bytes();

    let from_plain = parse_concept(plain, "notes/bom.md", ParserLimits::default()).unwrap();
    let from_bommed = parse_concept(bommed, "notes/bom.md", ParserLimits::default()).unwrap();

    assert_eq!(from_bommed, from_plain);
    // A mid-body U+FEFF is genuine content and must survive untouched.
    assert!(from_bommed.body_text.contains('\u{feff}'));
}

#[test]
fn column_zero_delimiter_inside_quoted_scalar_reports_invalid_frontmatter() {
    // Documented behavior (F16): a bare `---` on its own line inside a
    // multiline quoted scalar closes the frontmatter block early. The line
    // split does not re-implement YAML, so `serde_yaml` sees an unterminated
    // quoted scalar and the failure surfaces as InvalidFrontmatter whose
    // message pinpoints the offending line/column.
    let source = b"---\ntype: Note\ntitle: \"line one\n---\nline two\"\n---\nBody";

    let error = parse_concept(source, "notes/q.md", ParserLimits::default()).unwrap_err();

    assert!(
        matches!(&error, Error::InvalidFrontmatter { path, .. } if path == "notes/q.md"),
        "expected InvalidFrontmatter, got {error:?}"
    );
    assert_eq!(error.category(), ErrorCategory::Frontmatter);
    let message = error.to_string();
    assert!(
        message.contains("invalid YAML frontmatter") && message.contains("quoted scalar"),
        "error message should surface the unterminated quoted scalar: {message}"
    );
}
