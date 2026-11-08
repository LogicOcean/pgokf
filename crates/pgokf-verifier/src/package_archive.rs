//! Bounded release ZIP inspection and deterministic checksum table generation.
use std::{
    collections::BTreeMap,
    io::{Cursor, Read, Write},
    path::Path,
    time::Duration,
};
const MAX_ARCHIVE: u64 = 128 * 1024 * 1024;
/// Hash the sole package without ever extracting archive-controlled paths.
/// # Errors
/// Traversal, symlinks, duplicate package names, multiple packages and oversized data fail.
pub fn deb_digest(bytes: &[u8]) -> Result<(String, String), String> {
    if bytes.len() as u64 > MAX_ARCHIVE {
        return Err("archive size limit exceeded".into());
    }
    let mut archive = zip::ZipArchive::new(Cursor::new(bytes)).map_err(|e| e.to_string())?;
    central_entries(bytes, archive.central_directory_start(), archive.len())?;
    let mut package = None;
    let mut names = std::collections::BTreeSet::new();
    for index in 0..archive.len() {
        let mut file = archive.by_index(index).map_err(|e| e.to_string())?;
        let name = file.name().map_err(|e| e.to_string())?.into_owned();
        let path = file.enclosed_name().ok_or("archive path traversal")?;
        if !names.insert(name.clone()) {
            return Err("duplicate archive entry".into());
        }
        if file
            .unix_mode()
            .is_some_and(|mode| mode & 0o170_000 == 0o120_000)
        {
            return Err("archive symlink refused".into());
        }
        if path.extension().is_some_and(|s| s == "deb") {
            if package.is_some() {
                return Err("expected exactly one deb".into());
            }
            if path.components().count() != 1 {
                return Err("package must be at archive root".into());
            }
            if file.size() > MAX_ARCHIVE {
                return Err("package size limit exceeded".into());
            }
            let mut data = Vec::new();
            file.by_ref()
                .take(MAX_ARCHIVE + 1)
                .read_to_end(&mut data)
                .map_err(|e| e.to_string())?;
            if data.len() as u64 > MAX_ARCHIVE {
                return Err("package expanded size limit exceeded".into());
            }
            package = Some((name, crate::release_runtime::digest(&data)));
        }
    }
    package.ok_or_else(|| "expected exactly one deb".into())
}
// The ZIP reader indexes names and can collapse duplicates. Count the physical
// directory records before trusting that index, without extracting any paths.
fn central_entries(bytes: &[u8], start: u64, expected: usize) -> Result<(), String> {
    let mut offset = usize::try_from(start).map_err(|e| e.to_string())?;
    let mut count = 0;
    while bytes.get(offset..offset.saturating_add(4)) == Some(b"PK\x01\x02") {
        let header = bytes
            .get(offset..offset.saturating_add(46))
            .ok_or("truncated ZIP directory")?;
        let length = |index| usize::from(u16::from_le_bytes([header[index], header[index + 1]]));
        offset = offset
            .checked_add(46 + length(28) + length(30) + length(32))
            .ok_or("ZIP directory overflow")?;
        if offset > bytes.len() {
            return Err("truncated ZIP directory fields".into());
        }
        count += 1;
    }
    if count != expected {
        return Err("duplicate ZIP directory entries".into());
    }
    Ok(())
}

/// Download the complete supported major/architecture set and atomically replace
/// the table only after every archive has supplied its exact expected package.
/// # Errors
/// Download, archive validation, tuple mismatch or durable write failure fails.
pub fn checksums(version: &str, base_url: &str, output: &Path) -> Result<(), String> {
    if version.is_empty()
        || !version
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b".-".contains(&c))
    {
        return Err("invalid package version".into());
    }
    if !base_url.starts_with("https://") {
        return Err("HTTPS release root required".into());
    }
    let work = tempfile::tempdir().map_err(|e| e.to_string())?;
    let mut sums = BTreeMap::new();
    for major in [17, 18] {
        for arch in ["amd64", "arm64"] {
            let name = format!("pg-textsearch-v{version}-pg{major}-{arch}.zip");
            let path = work.path().join(&name);
            let mut spec = crate::release_runtime::command(
                work.path(),
                "curl",
                &[
                    "--fail",
                    "--silent",
                    "--show-error",
                    "--location",
                    "--retry",
                    "3",
                    "--max-filesize",
                    "134217728",
                    "--output",
                ],
            );
            spec.args.extend([
                path.clone().into_os_string(),
                format!("{base_url}/v{version}/{name}").into(),
            ]);
            spec.timeout = Duration::from_mins(5);
            spec.checked()?;
            let (name, digest) = deb_digest(&std::fs::read(path).map_err(|e| e.to_string())?)?;
            let expected = format!("pg-textsearch-postgresql-{major}_{version}-1_{arch}.deb");
            if name != expected || sums.insert(name, digest).is_some() {
                return Err("release package tuple mismatch".into());
            }
        }
    }
    let parent = output.parent().unwrap_or(Path::new("."));
    let mut file = tempfile::NamedTempFile::new_in(parent).map_err(|e| e.to_string())?;
    writeln!(file,"# Authenticated archive checksums for pg_textsearch {version}.\n# Regenerate: cargo run --locked -p pgokf-verifier -- package-checksums {version} packaging/docker/pg_textsearch.sha256").map_err(|e| e.to_string())?;
    for (name, digest) in sums {
        writeln!(file, "{digest}  {name}").map_err(|e| e.to_string())?;
    }
    file.as_file().sync_all().map_err(|e| e.to_string())?;
    file.persist(output).map_err(|e| e.to_string())?;
    std::fs::File::open(parent)
        .and_then(|f| f.sync_all())
        .map_err(|e| e.to_string())
}
#[cfg(test)]
mod tests {
    use super::*;
    fn zip(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut out = zip::ZipWriter::new(Cursor::new(Vec::new()));
        for (name, data) in entries {
            out.start_file(*name, zip::write::SimpleFileOptions::default())
                .unwrap();
            out.write_all(data).unwrap();
        }
        out.finish().unwrap().into_inner()
    }
    #[test]
    fn package_is_hashed_without_extraction() {
        let result = deb_digest(&zip(&[("a.deb", b"package"), ("README", b"doc")])).unwrap();
        assert_eq!(
            result,
            ("a.deb".into(), crate::release_runtime::digest(b"package"))
        );
    }
    #[test]
    fn traversal_multiple_nested_and_missing_packages_fail() {
        for entries in [
            vec![("../a.deb", &b"x"[..])],
            vec![("a.deb", &b"x"[..]), ("b.deb", &b"x"[..])],
            vec![("nested/a.deb", &b"x"[..])],
            vec![("README", &b"x"[..])],
        ] {
            assert!(deb_digest(&zip(&entries)).is_err());
        }
    }
    #[test]
    fn archive_metadata_guards_are_independent() {
        assert!(deb_digest(&zip(&[("a.deb", b"ok"), ("../notes", b"bad")])).is_err());
        let mut links = zip::ZipWriter::new(Cursor::new(Vec::new()));
        links
            .start_file("a.deb", zip::write::SimpleFileOptions::default())
            .unwrap();
        links.write_all(b"ok").unwrap();
        links
            .add_symlink("link", "a.deb", zip::write::SimpleFileOptions::default())
            .unwrap();
        assert!(deb_digest(&links.finish().unwrap().into_inner()).is_err());
        let mut duplicates = zip(&[("a.deb", b"ok"), ("notes", b"first"), ("other", b"second")]);
        for index in 0..duplicates.len() - 5 {
            if &duplicates[index..index + 5] == b"other" {
                duplicates[index..index + 5].copy_from_slice(b"notes");
            }
        }
        assert!(deb_digest(&duplicates).is_err());
        let mut oversized = zip(&[("a.deb", b"ok")]);
        let central = oversized
            .windows(4)
            .position(|s| s == b"PK\x01\x02")
            .unwrap();
        oversized[central + 24..central + 28]
            .copy_from_slice(&(u32::try_from(MAX_ARCHIVE).unwrap() + 1).to_le_bytes());
        assert!(deb_digest(&oversized).unwrap_err().contains("size limit"));
    }
}
