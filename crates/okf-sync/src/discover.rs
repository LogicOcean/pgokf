use std::path::{Path, PathBuf};

use glob::Pattern;
use stratify::logging::tracing;
use walkdir::WalkDir;

use crate::{Error, Result};

/// Include/exclude rules used during bundle discovery.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoverOptions {
    pub include: Vec<String>,
    pub exclude: Vec<String>,
    pub follow_symlinks: bool,
}

impl Default for DiscoverOptions {
    fn default() -> Self {
        Self {
            include: vec!["**/*.md".to_owned()],
            exclude: vec![
                ".git/**".to_owned(),
                "target/**".to_owned(),
                "node_modules/**".to_owned(),
            ],
            follow_symlinks: false,
        }
    }
}

/// A discovered Markdown file with absolute and normalized relative paths.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveredFile {
    pub absolute_path: PathBuf,
    pub relative_path: String,
}

/// Discover matching Markdown files in deterministic relative-path order.
///
/// # Errors
/// Returns an error when the root is invalid, a glob cannot be compiled, a
/// filesystem entry cannot be read, or a relative path is not UTF-8.
pub fn discover(root: &Path, options: &DiscoverOptions) -> Result<Vec<DiscoveredFile>> {
    if !root.is_dir() {
        return Err(Error::InvalidRoot(root.to_path_buf()));
    }
    let include = compile("include", &options.include)?;
    let exclude = compile("exclude", &options.exclude)?;
    let canonical_root = canonicalize(root)?;
    let mut files = Vec::new();

    for entry in WalkDir::new(root).follow_links(options.follow_symlinks) {
        let entry = entry?;
        if !entry.file_type().is_file() {
            continue;
        }
        let path = entry.path();
        if options.follow_symlinks && !canonicalize(path)?.starts_with(&canonical_root) {
            return Err(Error::SymlinkEscape(path.to_path_buf()));
        }
        if !path
            .extension()
            .and_then(|value| value.to_str())
            .is_some_and(|value| value.eq_ignore_ascii_case("md"))
        {
            continue;
        }
        let Ok(relative) = path.strip_prefix(root) else {
            continue;
        };
        let relative = relative
            .to_str()
            .ok_or_else(|| Error::NonUtf8Path(relative.to_path_buf()))?
            .replace('\\', "/");
        if matches_any(&include, &relative) && !matches_any(&exclude, &relative) {
            files.push(DiscoveredFile {
                absolute_path: path.to_path_buf(),
                relative_path: relative,
            });
        }
    }

    files.sort_by(|left, right| left.relative_path.cmp(&right.relative_path));

    tracing::debug!(root = %root.display(), file_count = files.len(), "discovered OKF files");
    Ok(files)
}

fn canonicalize(path: &Path) -> Result<PathBuf> {
    path.canonicalize().map_err(|source| Error::Io {
        path: path.to_path_buf(),
        source,
    })
}

fn compile(kind: &'static str, patterns: &[String]) -> Result<Vec<Pattern>> {
    patterns
        .iter()
        .map(|pattern| {
            Pattern::new(pattern).map_err(|source| Error::InvalidGlob {
                kind,
                pattern: pattern.clone(),
                source,
            })
        })
        .collect()
}

fn matches_any(patterns: &[Pattern], path: &str) -> bool {
    patterns.iter().any(|pattern| {
        pattern.matches(path)
            || pattern
                .as_str()
                .strip_prefix("**/")
                .is_some_and(|suffix| Pattern::new(suffix).is_ok_and(|value| value.matches(path)))
    })
}
