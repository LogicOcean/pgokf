// SPDX-License-Identifier: AGPL-3.0-only
//! Resolution of the links inside a rendered concept body.
//!
//! OKF bodies link to other concepts by file path: relative to the concept's
//! own directory (`appendix.md`, `../services/db.md`) or from the bundle
//! root (`/services/db.md`). At ingest the parser normalizes each destination
//! ([`okf_parser::resolve_reference`]) and the catalog records the edge in
//! `pgokf.links` (normalized bundle-relative path to target id). The page
//! reuses both: the same normalization, so it can never drift from ingest,
//! and the catalog's resolution, so an href becomes a concept page only when
//! the catalog resolved that path, and a path the catalog could not resolve
//! is rendered as text rather than as a link that would 404.

use std::collections::HashMap;

/// What to render for one body link destination.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum LinkTarget {
    /// Not a bundle-internal concept reference (external URL, in-page anchor,
    /// a non-Markdown file): render the href as written.
    Keep,
    /// A concept the catalog resolved: link to its page (fragment kept).
    Page(String),
    /// A concept path the catalog could not resolve: render as text.
    Dead,
}

/// Rewrites body hrefs to concept pages using the catalog's own resolution.
pub(crate) struct Resolver<'a> {
    concept_path: &'a str,
    /// Normalized target path to the concept page href.
    targets: HashMap<&'a str, String>,
}

impl<'a> Resolver<'a> {
    /// `targets` maps each resolved outgoing link's normalized `target_path`
    /// to the href of its concept page.
    pub(crate) fn new(concept_path: &'a str, targets: HashMap<&'a str, String>) -> Self {
        Self {
            concept_path,
            targets,
        }
    }

    /// Classify one body href.
    pub(crate) fn resolve(&self, href: &str) -> LinkTarget {
        let href = href.trim();
        if href.is_empty() || href.starts_with('#') {
            return LinkTarget::Keep;
        }
        let reference = okf_parser::resolve_reference(href, self.concept_path);
        if reference.is_external {
            return LinkTarget::Keep;
        }
        let Some(path) = reference.target_path else {
            // Empty, escaping the bundle, or not a Markdown file: the catalog
            // records no concept edge for it, so leave it as written.
            return LinkTarget::Keep;
        };
        let Some(page) = self.targets.get(path.as_str()) else {
            return LinkTarget::Dead;
        };
        LinkTarget::Page(match href.split_once('#') {
            Some((_, fragment)) if !fragment.is_empty() => format!("{page}#{fragment}"),
            _ => page.clone(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn resolver(concept_path: &str) -> Resolver<'_> {
        Resolver::new(
            concept_path,
            HashMap::from([
                (
                    "services/postgresql.md",
                    "/concepts/1/services/postgresql".to_owned(),
                ),
                (
                    "runbooks/database-failover.md",
                    "/concepts/1/runbooks/database-failover".to_owned(),
                ),
            ]),
        )
    }

    #[test]
    fn resolve_maps_relative_root_and_extension_less_forms_like_the_parser() {
        // Arrange
        let r = resolver("runbooks/appendix.md");

        // Act & Assert
        assert_eq!(
            r.resolve("../services/postgresql.md#slots"),
            LinkTarget::Page("/concepts/1/services/postgresql#slots".to_owned())
        );
        assert_eq!(
            r.resolve("/services/postgresql.MD"),
            LinkTarget::Page("/concepts/1/services/postgresql".to_owned())
        );
        assert_eq!(
            r.resolve("database-failover"),
            LinkTarget::Page("/concepts/1/runbooks/database-failover".to_owned())
        );
        assert_eq!(
            r.resolve("..\\services\\postgresql.md"),
            LinkTarget::Page("/concepts/1/services/postgresql".to_owned())
        );
    }

    #[test]
    fn resolve_marks_unresolved_concept_paths_dead_and_keeps_the_rest() {
        // Arrange
        let r = resolver("runbooks/appendix.md");

        // Act & Assert
        assert_eq!(r.resolve("capacity-planning.md"), LinkTarget::Dead);
        for href in [
            "https://example.test/x.md",
            "mailto:oncall@example.test",
            "//cdn.example.test/a.md",
            "#section",
            "",
            "../../escapes.md",
            "queries/search.sql",
        ] {
            assert_eq!(r.resolve(href), LinkTarget::Keep, "{href}");
        }
    }
}
