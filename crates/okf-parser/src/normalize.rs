use std::path::{Component, Path};

use crate::{Error, Result};

/// Normalize a bundle-relative concept path to UTF-8 slash separators.
///
/// # Errors
/// Returns an error for empty, absolute, traversing, non-UTF-8, or
/// non-Markdown paths.
pub fn normalize_path(path: &Path) -> Result<String> {
    let raw = path.to_str().ok_or(Error::NonUtf8Path)?.replace('\\', "/");
    if raw.is_empty() {
        return Err(Error::EmptyPath);
    }
    if raw.starts_with('/') || has_windows_prefix(&raw) {
        return Err(Error::AbsolutePath(raw));
    }

    let mut parts = Vec::new();
    for component in Path::new(&raw).components() {
        match component {
            Component::Normal(part) => parts.push(part.to_string_lossy().into_owned()),
            Component::CurDir => {}
            Component::ParentDir => return Err(Error::PathTraversal(raw)),
            Component::RootDir | Component::Prefix(_) => return Err(Error::AbsolutePath(raw)),
        }
    }
    if parts.is_empty() {
        return Err(Error::EmptyPath);
    }

    let mut normalized = parts.join("/");
    match Path::new(&normalized)
        .extension()
        .and_then(|ext| ext.to_str())
    {
        None => normalized.push_str(".md"),
        Some(ext) if ext.eq_ignore_ascii_case("md") => {
            let length_without_extension = normalized.len() - ext.len();
            normalized.replace_range(length_without_extension.., "md");
        }
        Some(_) => return Err(Error::UnsupportedExtension(normalized)),
    }
    Ok(normalized)
}

fn has_windows_prefix(path: &str) -> bool {
    let bytes = path.as_bytes();
    bytes.len() >= 3 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' && bytes[2] == b'/'
}
