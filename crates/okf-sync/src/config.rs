// SPDX-License-Identifier: AGPL-3.0-only
//! Scan configuration for bundle discovery.

use std::path::PathBuf;

/// Configuration for a filesystem scan.
///
/// `include` and `exclude` are glob patterns relative to [`Self::root`]. When no
/// include patterns are supplied, all `**/*.md` files are included. Excludes always
/// take precedence over includes.
///
/// The optional limits guard a scan against runaway bundles: [`Self::max_file_bytes`]
/// bounds the size of any single document and [`Self::max_files`] bounds how many
/// documents one scan may yield. In the `PostgreSQL` extension these are populated from
/// the `pgokf.max_file_bytes` and `pgokf.max_bundle_files` server settings.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SyncConfig {
    /// Bundle root directory that discovery walks.
    pub root: PathBuf,
    /// Glob patterns selecting documents, relative to [`Self::root`].
    pub include: Vec<String>,
    /// Glob patterns removing documents from the selection; they win over includes.
    pub exclude: Vec<String>,
    /// Upper bound on a single discovered file's size in bytes; `None` is unlimited.
    pub max_file_bytes: Option<u64>,
    /// Upper bound on the number of discovered files; `None` is unlimited.
    pub max_files: Option<usize>,
}

impl SyncConfig {
    /// Create a configuration that discovers every Markdown file under `root`
    /// with no exclusions and no resource limits.
    #[must_use]
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            include: vec!["**/*.md".to_owned()],
            exclude: Vec::new(),
            max_file_bytes: None,
            max_files: None,
        }
    }

    /// Replace the include patterns.
    #[must_use]
    pub fn with_include(mut self, patterns: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.include = patterns.into_iter().map(Into::into).collect();
        self
    }

    /// Replace the exclude patterns.
    #[must_use]
    pub fn with_exclude(mut self, patterns: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.exclude = patterns.into_iter().map(Into::into).collect();
        self
    }

    /// Reject any discovered file larger than `limit` bytes.
    #[must_use]
    pub fn with_max_file_bytes(mut self, limit: u64) -> Self {
        self.max_file_bytes = Some(limit);
        self
    }

    /// Reject a scan that discovers more than `limit` files.
    #[must_use]
    pub fn with_max_files(mut self, limit: usize) -> Self {
        self.max_files = Some(limit);
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_defaults_to_all_markdown_with_no_limits() {
        let config = SyncConfig::new("/bundles/handbook");

        assert_eq!(config.root, PathBuf::from("/bundles/handbook"));
        assert_eq!(config.include, vec!["**/*.md".to_owned()]);
        assert!(config.exclude.is_empty());
        assert_eq!(config.max_file_bytes, None);
        assert_eq!(config.max_files, None);
    }

    #[test]
    fn builder_methods_replace_patterns_and_set_limits() {
        let config = SyncConfig::new("/bundles/handbook")
            .with_include(["docs/**/*.md"])
            .with_exclude(["**/draft-*.md"])
            .with_max_file_bytes(1_048_576)
            .with_max_files(500);

        assert_eq!(config.include, vec!["docs/**/*.md".to_owned()]);
        assert_eq!(config.exclude, vec!["**/draft-*.md".to_owned()]);
        assert_eq!(config.max_file_bytes, Some(1_048_576));
        assert_eq!(config.max_files, Some(500));
    }
}
