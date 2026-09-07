// SPDX-License-Identifier: AGPL-3.0-only
//! Delivery of a built tree: a deterministic zip archive, or files written
//! under a directory.

use std::io::{Cursor, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, DateTime, ZipWriter};

use crate::plugin::Plugin;

/// The tree as a zip archive to unpack at the workspace root. Timestamps are
/// fixed, so an unchanged tree zips to identical bytes.
///
/// # Errors
///
/// A zip encoding failure.
pub fn zip(plugin: &Plugin) -> Result<Vec<u8>> {
    let mut writer = ZipWriter::new(Cursor::new(Vec::new()));
    let options = SimpleFileOptions::default()
        .compression_method(CompressionMethod::Deflated)
        .last_modified_time(DateTime::default())
        .unix_permissions(0o644);
    for file in &plugin.files {
        let options = if file.executable {
            options.unix_permissions(0o755)
        } else {
            options
        };
        writer
            .start_file(&file.path, options)
            .with_context(|| format!("adding {} to the archive", file.path))?;
        writer
            .write_all(&file.bytes)
            .with_context(|| format!("writing {} to the archive", file.path))?;
    }
    let cursor = writer.finish().context("finishing the archive")?;
    Ok(cursor.into_inner())
}

/// Write the tree under `dir`, creating directories as needed. Every path
/// is validated before anything is written: it must stay inside `dir`, no
/// existing ancestor or target may be a symbolic link, and an existing file
/// is refused unless `overwrite` is set. A failure while writing reports
/// how many files were already written.
///
/// # Errors
///
/// A path that escapes `dir` or carries a separator or control character,
/// a symbolic link on the way, an existing file without `overwrite`, or an
/// I/O failure.
pub fn write_to_dir(plugin: &Plugin, dir: &Path, overwrite: bool) -> Result<Vec<PathBuf>> {
    // Pass one: validate everything, touch nothing.
    let mut targets = Vec::with_capacity(plugin.files.len());
    for file in &plugin.files {
        let relative = safe_relative(&file.path)?;
        refuse_symlinks(dir, &relative)?;
        let target = dir.join(&relative);
        match std::fs::symlink_metadata(&target) {
            Ok(meta) if meta.file_type().is_symlink() => {
                return Err(anyhow!(
                    "{} is a symbolic link; refusing to write through it",
                    target.display()
                ));
            }
            Ok(meta) if meta.is_dir() => {
                return Err(anyhow!("{} is a directory", target.display()));
            }
            Ok(_) if !overwrite => {
                return Err(anyhow!(
                    "{} exists; pass overwrite to replace it",
                    target.display()
                ));
            }
            _ => {}
        }
        targets.push(target);
    }
    // Pass two: write.
    let mut written = Vec::with_capacity(targets.len());
    for (file, target) in plugin.files.iter().zip(targets) {
        if let Err(error) = write_one(&target, &file.bytes, overwrite, file.executable) {
            return Err(error.context(format!(
                "after writing {} of {} files",
                written.len(),
                plugin.files.len()
            )));
        }
        written.push(target);
    }
    Ok(written)
}

fn write_one(target: &Path, bytes: &[u8], overwrite: bool, executable: bool) -> Result<()> {
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    let mut options = std::fs::OpenOptions::new();
    options.write(true);
    if overwrite {
        options.create(true).truncate(true);
    } else {
        // Atomic with the earlier check: a file that appeared meanwhile is
        // refused rather than replaced.
        options.create_new(true);
    }
    let mut handle = options
        .open(target)
        .with_context(|| format!("opening {}", target.display()))?;
    handle
        .write_all(bytes)
        .with_context(|| format!("writing {}", target.display()))?;
    #[cfg(unix)]
    if executable {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(target, std::fs::Permissions::from_mode(0o755))
            .with_context(|| format!("marking {} executable", target.display()))?;
    }
    #[cfg(not(unix))]
    let _ = executable;
    Ok(())
}

/// No existing ancestor of `relative` inside `dir` may be a symbolic link,
/// or a tree could be redirected outside the workspace.
fn refuse_symlinks(dir: &Path, relative: &Path) -> Result<()> {
    let mut current = dir.to_path_buf();
    for component in relative.components() {
        current.push(component);
        match std::fs::symlink_metadata(&current) {
            Ok(meta) if meta.file_type().is_symlink() => {
                return Err(anyhow!(
                    "{} is a symbolic link; refusing to write beneath it",
                    current.display()
                ));
            }
            _ => {}
        }
    }
    Ok(())
}

/// A tree path as a relative path of plain segments: no `.`/`..`, no
/// absolute or drive-prefixed form, no backslash (a separator on Windows
/// extractors), no control characters.
fn safe_relative(path: &str) -> Result<PathBuf> {
    if path.is_empty() || path.chars().any(|c| c == '\\' || c.is_control()) || path.starts_with('/')
    {
        return Err(anyhow!("refusing to write outside the workspace: {path:?}"));
    }
    let mut out = PathBuf::new();
    for segment in path.split('/') {
        if segment.is_empty() || segment == "." || segment == ".." || segment.contains(':') {
            return Err(anyhow!("refusing to write outside the workspace: {path:?}"));
        }
        out.push(segment);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugin::PluginFile;

    fn plugin() -> Plugin {
        Plugin {
            target: "generic".to_owned(),
            name: "t".to_owned(),
            root: "okf-knowledge".to_owned(),
            files: vec![PluginFile {
                path: "okf-knowledge/INDEX.md".to_owned(),
                bytes: b"# t\n".to_vec(),
                sha256: String::new(),
                executable: false,
            }],
            concept_count: 0,
            package_count: 0,
            concepts: Vec::new(),
        }
    }

    #[test]
    fn zip_is_deterministic_and_contains_the_files() {
        // Arrange
        let plugin = plugin();

        // Act
        let first = zip(&plugin).expect("zips");
        let second = zip(&plugin).expect("zips");

        // Assert
        assert_eq!(first, second);
        let mut archive = zip::ZipArchive::new(Cursor::new(first)).expect("valid zip");
        assert_eq!(archive.len(), 1);
        assert_eq!(
            archive.by_index(0).expect("entry").name(),
            "okf-knowledge/INDEX.md"
        );
    }

    #[test]
    fn safe_relative_accepts_plain_trees_and_refuses_everything_else() {
        // Arrange & Act & Assert
        assert!(safe_relative("okf-knowledge/a/b.md").is_ok());
        for bad in [
            "",
            "/etc/passwd",
            "../x.md",
            "a/../x.md",
            "a/./x.md",
            "a//x.md",
            "a\\..\\x.md",
            "C:/x.md",
            "a/x\u{0}.md",
        ] {
            assert!(safe_relative(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn write_to_dir_validates_everything_before_writing_anything() {
        // Arrange: the second file already exists, so nothing may be written.
        let dir =
            std::env::temp_dir().join(format!("pgokf-workspace-atomic-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("dir");
        std::fs::write(dir.join("okf-workspace.lock"), b"old").expect("seed");
        let mut two = plugin();
        two.files.push(PluginFile {
            path: "okf-workspace.lock".to_owned(),
            bytes: b"new".to_vec(),
            sha256: String::new(),
            executable: false,
        });

        // Act
        let result = write_to_dir(&two, &dir, false);

        // Assert
        assert!(result.is_err());
        assert!(
            !dir.join("okf-knowledge/INDEX.md").exists(),
            "first file must not be written"
        );
        assert_eq!(
            std::fs::read(dir.join("okf-workspace.lock")).expect("read"),
            b"old"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn write_to_dir_refuses_symbolic_links_even_with_overwrite() {
        // Arrange: a dangling link where the index would go.
        let dir =
            std::env::temp_dir().join(format!("pgokf-workspace-symlink-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("okf-knowledge")).expect("dir");
        std::os::unix::fs::symlink("../../escaped.md", dir.join("okf-knowledge/INDEX.md"))
            .expect("link");

        // Act
        let result = write_to_dir(&plugin(), &dir, true);

        // Assert
        assert!(result.is_err());
        assert!(!dir.join("../escaped.md").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn write_to_dir_refuses_escapes_and_existing_files() {
        // Arrange
        let dir = std::env::temp_dir().join(format!("pgokf-workspace-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut escaping = plugin();
        escaping.files[0].path = "../outside.md".to_owned();

        // Act
        let written = write_to_dir(&plugin(), &dir, false).expect("writes");
        let again = write_to_dir(&plugin(), &dir, false);
        let overwritten = write_to_dir(&plugin(), &dir, true);
        let escape = write_to_dir(&escaping, &dir, true);

        // Assert
        assert_eq!(written.len(), 1);
        assert!(written[0].ends_with("okf-knowledge/INDEX.md"));
        assert!(again.is_err());
        assert!(overwritten.is_ok());
        assert!(escape.is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
