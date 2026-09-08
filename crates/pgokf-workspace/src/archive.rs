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

/// Write the tree under `dir`, creating directories as needed.
///
/// Three passes, so a failure leaves the destination as it was: every path
/// is validated first (it must stay inside `dir`, no existing ancestor or
/// target may be a symbolic link, and an existing file is refused unless
/// `overwrite` is set), then every file is written beside its destination
/// under a temporary name, and only then is each renamed into place. If
/// anything fails while writing, the temporary files are removed and
/// nothing of the tree is left behind - where writing in place used to
/// leave the first ten files of eleven.
///
/// Each temporary is created with `create_new`, so a symbolic link planted
/// between the passes is refused rather than written through.
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
    // Pass two: write every file beside its destination, so a failure here
    // leaves nothing of the tree behind. Validating first and then writing
    // in place still half-wrote a tree when the eleventh file failed.
    let mut staged: Vec<(PathBuf, PathBuf)> = Vec::with_capacity(targets.len());
    let cleanup = |staged: &[(PathBuf, PathBuf)]| {
        for (temp, _) in staged {
            let _ = std::fs::remove_file(temp);
        }
    };
    for (file, target) in plugin.files.iter().zip(targets) {
        let temp = staging_path(&target);
        if let Err(error) = write_one(&temp, &file.bytes, file.executable) {
            cleanup(&staged);
            return Err(error.context("no file of the tree was written"));
        }
        staged.push((temp, target));
    }
    // Pass three: put each in place. A rename cannot fail for want of space
    // or permission at this point, so the window in which the tree is part
    // old and part new is as small as the filesystem allows.
    let mut written = Vec::with_capacity(staged.len());
    for (temp, target) in &staged {
        if let Err(error) = std::fs::rename(temp, target) {
            cleanup(&staged[written.len()..]);
            return Err(anyhow!(
                "renaming {} into place: {error} (after {} of {} files)",
                target.display(),
                written.len(),
                plugin.files.len()
            ));
        }
        written.push(target.clone());
    }
    Ok(written)
}

/// The prefix a file wears while it is staged beside its destination. A
/// plugin file may not itself use it: its staging temp would otherwise land
/// on the destination of the plugin's own `.okf-tmp-…` file, and the two
/// would clobber each other in the rename pass. [`safe_relative`] refuses it.
const STAGING_PREFIX: &str = ".okf-tmp-";

/// Where a file is written before it takes its name: beside the
/// destination, so the rename is on one filesystem.
fn staging_path(target: &Path) -> PathBuf {
    let mut name = std::ffi::OsString::from(STAGING_PREFIX);
    name.push(target.file_name().unwrap_or_default());
    target.with_file_name(name)
}

/// Write one staged file. `create_new` throughout: the staging name is
/// this process's to make, so anything already there - including a symbolic
/// link an attacker planted between the passes - is refused rather than
/// written through. That is what closes the window the old `overwrite`
/// path left open, where the open followed a link to anywhere the process
/// could reach.
fn write_one(target: &Path, bytes: &[u8], executable: bool) -> Result<()> {
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
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
        // Trailing spaces and dots are stripped by Windows path handling,
        // so a segment of `".. "` climbs a directory there; judge the
        // segment as that platform would see it.
        let squared = segment.trim_end_matches([' ', '.']);
        if segment.is_empty()
            || squared.is_empty()
            || segment == "."
            || squared == ".."
            || segment.contains(':')
            || segment.starts_with(STAGING_PREFIX)
        {
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
            // The reserved staging prefix: its temp would clobber a sibling.
            "okf-knowledge/.okf-tmp-a.md",
            ".okf-tmp-INDEX.md",
        ] {
            assert!(safe_relative(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn a_file_using_the_staging_prefix_is_refused_before_it_can_clobber_a_sibling() {
        // Arrange: a plugin holding both `a.md` and `.okf-tmp-a.md` - the
        // latter's destination is the former's staging temp, so writing them
        // naively lost one file. It must be refused, whole, up front.
        let dir = std::env::temp_dir().join(format!("pgokf-staging-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut clash = plugin();
        clash.files[0].path = "okf-knowledge/a.md".to_owned();
        clash.files.push(PluginFile {
            path: "okf-knowledge/.okf-tmp-a.md".to_owned(),
            bytes: b"DOTFILE".to_vec(),
            sha256: String::new(),
            executable: false,
        });

        // Act
        let outcome = write_to_dir(&clash, &dir, true);

        // Assert: refused, and nothing of the tree written.
        assert!(outcome.is_err(), "{outcome:?}");
        assert!(walk(&dir).is_empty());
        let _ = std::fs::remove_dir_all(&dir);
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

    #[test]
    fn a_refused_write_leaves_the_destination_untouched() {
        // Arrange: two files, the second at a path the first has made a
        // directory, so the write fails partway.
        let dir = std::env::temp_dir().join(format!("pgokf-atomic-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut broken = plugin();
        broken.files.push(PluginFile {
            path: "okf-knowledge/INDEX.md/nested.md".to_owned(),
            bytes: b"x".to_vec(),
            sha256: String::new(),
            executable: false,
        });

        // Act
        let outcome = write_to_dir(&broken, &dir, false);

        // Assert: refused, and nothing of the tree - not even the file that
        // wrote cleanly - is on disk.
        assert!(outcome.is_err(), "{outcome:?}");
        let left: Vec<_> = walk(&dir);
        assert!(left.is_empty(), "{left:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Every file under `dir`, for asserting that none was left behind.
    fn walk(dir: &Path) -> Vec<PathBuf> {
        let mut out = Vec::new();
        let mut stack = vec![dir.to_path_buf()];
        while let Some(next) = stack.pop() {
            let Ok(entries) = std::fs::read_dir(&next) else {
                continue;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    stack.push(path);
                } else {
                    out.push(path);
                }
            }
        }
        out
    }
}
