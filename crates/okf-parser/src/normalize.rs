use std::path::{Component, Path};

use crate::{Error, Result};

/// File names reserved by OKF; they describe a directory, not a concept.
const RESERVED_FILE_NAMES: [&str; 2] = ["index.md", "log.md"];

/// Normalize a bundle-relative concept path to UTF-8 slash separators.
///
/// The result keeps its `.md` suffix (lowercased); use [`concept_id`] to
/// derive the OKF concept ID from it.
///
/// # Errors
/// Returns an error for empty, absolute, traversing, non-UTF-8, or
/// non-Markdown paths.
pub fn normalize_path(path: &Path) -> Result<String> {
    let raw = path
        .to_str()
        .ok_or_else(|| Error::NonUtf8Path {
            path: path.to_string_lossy().into_owned(),
        })?
        .replace('\\', "/");
    if raw.is_empty() {
        return Err(Error::EmptyPath);
    }
    if raw.starts_with('/') || has_windows_prefix(&raw) {
        return Err(Error::AbsolutePath { path: raw });
    }

    let mut parts = Vec::new();
    for component in Path::new(&raw).components() {
        match component {
            Component::Normal(part) => parts.push(part.to_string_lossy().into_owned()),
            Component::CurDir => {}
            Component::ParentDir => return Err(Error::PathTraversal { path: raw }),
            Component::RootDir | Component::Prefix(_) => {
                return Err(Error::AbsolutePath { path: raw });
            }
        }
    }
    if parts.is_empty() {
        return Err(Error::EmptyPath);
    }

    apply_markdown_extension(parts.join("/"))
}

/// Derive the OKF concept ID from a normalized bundle-relative path.
///
/// Per the architecture invariants, a concept ID is the bundle-relative path
/// without its `.md` suffix. The ID is always path-derived; a
/// producer-declared `id` never overrides it.
#[must_use]
pub fn concept_id(normalized_path: &str) -> String {
    normalized_path
        .strip_suffix(".md")
        .unwrap_or(normalized_path)
        .to_owned()
}

/// Whether a normalized bundle-relative path names a reserved OKF file.
///
/// `index.md` and `log.md` are reserved at every directory level: they carry
/// bundle/directory bookkeeping and must never become ordinary concepts.
/// Callers should filter reserved files with this predicate before parsing;
/// [`crate::parse_concept`] enforces the invariant defensively by rejecting
/// them with [`Error::ReservedPath`].
#[must_use]
pub fn is_reserved_path(normalized_path: &str) -> bool {
    let file_name = normalized_path
        .rsplit_once('/')
        .map_or(normalized_path, |(_, name)| name);
    RESERVED_FILE_NAMES.contains(&file_name)
}

/// Resolve an internal Markdown link destination to a normalized
/// bundle-relative path, applying the same rules as concept paths.
///
/// `source_path` must be the normalized path of the document containing the
/// link. Destinations starting with `/` resolve from the bundle root; all
/// others resolve from the source document's directory. Fragments (`#...`)
/// are stripped and never change the target; a fragment-only destination
/// resolves to the source document itself. The `.md` suffix is preserved and
/// appended when the destination has no extension.
///
/// Returns `None` when the destination cannot name a concept: it escapes
/// above the bundle root, resolves to nothing, or has a non-Markdown
/// extension. Existence checks against actual bundle contents belong to the
/// sync layer, not the parser.
#[must_use]
pub fn resolve_link_target(target: &str, source_path: &str) -> Option<String> {
    let without_fragment = target.replace('\\', "/");
    let without_fragment = without_fragment.split('#').next().unwrap_or_default();
    if without_fragment.is_empty() {
        // Fragment-only (or empty) destinations point at the source document.
        return Some(source_path.to_owned());
    }

    let (mut parts, remainder) = match without_fragment.strip_prefix('/') {
        Some(rooted) => (Vec::new(), rooted),
        None => (parent_components(source_path), without_fragment),
    };
    for component in remainder.split('/') {
        match component {
            "" | "." => {}
            ".." => {
                // Popping past the bundle root escapes it: unresolvable.
                parts.pop()?;
            }
            part => parts.push(part.to_owned()),
        }
    }
    if parts.is_empty() {
        return None;
    }

    apply_markdown_extension(parts.join("/")).ok()
}

/// Enforce the concept extension rule: append `.md` when the path has no
/// extension, lowercase an existing `md`/`MD` suffix, reject anything else.
fn apply_markdown_extension(mut normalized: String) -> Result<String> {
    match Path::new(&normalized)
        .extension()
        .and_then(|ext| ext.to_str())
    {
        None => normalized.push_str(".md"),
        Some(ext) if ext.eq_ignore_ascii_case("md") => {
            let length_without_extension = normalized.len() - ext.len();
            normalized.replace_range(length_without_extension.., "md");
        }
        Some(_) => return Err(Error::UnsupportedExtension { path: normalized }),
    }
    Ok(normalized)
}

/// Directory components of a normalized bundle-relative file path.
fn parent_components(normalized_path: &str) -> Vec<String> {
    normalized_path
        .rsplit_once('/')
        .map(|(directory, _)| directory.split('/').map(str::to_owned).collect())
        .unwrap_or_default()
}

fn has_windows_prefix(path: &str) -> bool {
    let bytes = path.as_bytes();
    bytes.len() >= 3 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' && bytes[2] == b'/'
}

#[cfg(test)]
mod tests {
    use super::{concept_id, is_reserved_path, resolve_link_target};

    #[test]
    fn concept_id_strips_md_suffix_only() {
        assert_eq!(concept_id("notes/hello.md"), "notes/hello");
        assert_eq!(concept_id("hello.md"), "hello");
        assert_eq!(concept_id("already-stripped"), "already-stripped");
    }

    #[test]
    fn is_reserved_path_matches_index_and_log_at_any_depth() {
        assert!(is_reserved_path("index.md"));
        assert!(is_reserved_path("log.md"));
        assert!(is_reserved_path("nested/deeper/index.md"));
        assert!(is_reserved_path("nested/log.md"));
        assert!(!is_reserved_path("nested/reindex.md"));
        assert!(!is_reserved_path("catalog.md"));
    }

    #[test]
    fn resolve_link_target_resolves_relative_to_source_directory() {
        let resolved = resolve_link_target("../services/postgresql.md", "dashboards/health.md");
        assert_eq!(resolved.as_deref(), Some("services/postgresql.md"));

        let sibling = resolve_link_target("./appendix.md", "runbooks/failover.md");
        assert_eq!(sibling.as_deref(), Some("runbooks/appendix.md"));
    }

    #[test]
    fn resolve_link_target_resolves_leading_slash_from_bundle_root() {
        let resolved = resolve_link_target("/services/postgresql.md", "runbooks/failover.md");
        assert_eq!(resolved.as_deref(), Some("services/postgresql.md"));
    }

    #[test]
    fn resolve_link_target_strips_fragments_and_keeps_self_reference() {
        assert_eq!(
            resolve_link_target("other.md#section", "notes/a.md").as_deref(),
            Some("notes/other.md")
        );
        assert_eq!(
            resolve_link_target("#top", "notes/a.md").as_deref(),
            Some("notes/a.md")
        );
    }

    #[test]
    fn resolve_link_target_appends_md_when_extension_missing() {
        assert_eq!(
            resolve_link_target("sibling", "notes/a.md").as_deref(),
            Some("notes/sibling.md")
        );
    }

    #[test]
    fn resolve_link_target_rejects_bundle_escapes_and_non_markdown() {
        assert_eq!(resolve_link_target("../../escape.md", "notes/a.md"), None);
        assert_eq!(resolve_link_target("/../escape.md", "notes/a.md"), None);
        assert_eq!(resolve_link_target("diagram.png", "notes/a.md"), None);
    }
}
