// SPDX-License-Identifier: AGPL-3.0-only
//! Integration tests over the repository's shared bundle fixtures
//! (`tests/bundles/`), exercising link normalization and Unicode handling
//! against real files.

use std::path::{Path, PathBuf};

use okf_parser::{ParsedConcept, ParserLimits, parse_concept};

/// Absolute path of a fixture inside the repository's `tests/bundles/` tree.
fn bundle_root(bundle: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/bundles")
        .join(bundle)
}

/// Read and parse one fixture using its bundle-relative path.
fn parse_fixture(bundle: &str, relative_path: &str) -> ParsedConcept {
    let absolute = bundle_root(bundle).join(relative_path);
    let source = std::fs::read(&absolute)
        .unwrap_or_else(|error| panic!("cannot read fixture {}: {error}", absolute.display()));
    parse_concept(&source, relative_path, ParserLimits::default())
        .unwrap_or_else(|error| panic!("cannot parse fixture {relative_path}: {error}"))
}

#[test]
fn links_bundle_root_relative_targets_resolve_from_bundle_root() {
    let parsed = parse_fixture("links", "overview.md");

    assert_eq!(parsed.id, "overview");
    assert_eq!(parsed.links.len(), 2);
    let service = &parsed.links[0];
    assert_eq!(service.target, "/services/postgresql.md");
    assert!(!service.is_external);
    assert_eq!(
        service.target_path.as_deref(),
        Some("services/postgresql.md")
    );
    assert_eq!(service.target_id.as_deref(), Some("services/postgresql"));
    let runbook = &parsed.links[1];
    assert_eq!(
        runbook.target_path.as_deref(),
        Some("runbooks/database-failover.md")
    );
    assert_eq!(
        runbook.target_id.as_deref(),
        Some("runbooks/database-failover")
    );
}

#[test]
fn links_bundle_parent_relative_targets_resolve_from_source_directory() {
    let parsed = parse_fixture("links", "dashboards/database-health.md");

    assert_eq!(parsed.id, "dashboards/database-health");
    assert_eq!(parsed.links.len(), 2);
    let internal = &parsed.links[0];
    assert_eq!(internal.target, "../services/postgresql.md");
    assert!(!internal.is_external);
    assert_eq!(
        internal.target_path.as_deref(),
        Some("services/postgresql.md")
    );
    assert_eq!(internal.target_id.as_deref(), Some("services/postgresql"));
    let external = &parsed.links[1];
    assert_eq!(external.target, "https://status.example.test/");
    assert!(external.is_external);
    assert_eq!(external.target_path, None);
    assert_eq!(external.target_id, None);
}

#[test]
fn links_bundle_normalizes_unresolved_targets_for_sync_layer_resolution() {
    let parsed = parse_fixture("links", "runbooks/database-failover.md");

    assert_eq!(parsed.id, "runbooks/database-failover");
    assert_eq!(parsed.links.len(), 3);
    assert_eq!(
        parsed.links[0].target_path.as_deref(),
        Some("dashboards/database-health.md")
    );
    assert_eq!(
        parsed.links[1].target_path.as_deref(),
        Some("services/postgresql.md")
    );
    // The appendix file does not exist; the parser still normalizes the
    // target, and the sync layer decides resolvability against the bundle.
    let unresolved = &parsed.links[2];
    assert_eq!(unresolved.target, "./recovery-appendix.md");
    assert!(!unresolved.is_external);
    assert_eq!(
        unresolved.target_path.as_deref(),
        Some("runbooks/recovery-appendix.md")
    );
    assert_eq!(
        unresolved.target_id.as_deref(),
        Some("runbooks/recovery-appendix")
    );
}

#[test]
fn unicode_bundle_preserves_cjk_frontmatter_and_body() {
    let parsed = parse_fixture("unicode", "cjk.md");

    assert_eq!(parsed.id, "cjk");
    assert_eq!(parsed.title, "数据库故障排除");
    assert_eq!(
        parsed.description.as_deref(),
        Some("PostgreSQL データベース 장애 대응 지침")
    );
    assert_eq!(parsed.tags, ["中文", "日本語", "한국어"]);
    assert!(parsed.body_text.contains("连接失败时，请先检查网络。"));
    assert!(
        parsed
            .body_text
            .contains("장애가 계속되면 담당자에게 알리세요.")
    );
}

#[test]
fn unicode_bundle_preserves_emoji_sequences_exactly() {
    let parsed = parse_fixture("unicode", "emoji.md");

    assert_eq!(parsed.id, "emoji");
    assert_eq!(parsed.title, "🚨 Incident response: café latency");
    assert_eq!(parsed.tags, ["on-call", "⚡", "café"]);
    assert!(parsed.body_text.contains("api_🚀"));
    assert!(parsed.body_text.contains("👩🏽\u{200d}💻"));
    assert!(parsed.body_text.contains("🇯🇵"));
}

#[test]
fn unicode_bundle_preserves_rtl_text_without_rewriting() {
    let parsed = parse_fixture("unicode", "rtl.md");

    assert_eq!(parsed.id, "rtl");
    assert_eq!(parsed.title, "دليل الاستجابة للحوادث");
    assert_eq!(parsed.tags, ["العربية", "עברית", "RTL"]);
    assert!(parsed.body_text.contains("בדקו את היומנים"));
    assert!(parsed.body_text.contains("PostgreSQL الإصدار 17 - גרסה 17."));
}
