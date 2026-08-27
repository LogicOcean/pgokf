//! Integration tests exercising the public API end to end: incremental sync
//! reporting, hashing helpers, and the symlink containment policy.

use std::fs;

use okf_sync::{SyncConfig, SyncError, SyncReport, build_plan, discover, hash_bytes, hash_file};
use tempfile::{TempDir, tempdir};

fn write(root: &TempDir, relative: &str, contents: &str) {
    let path = root.path().join(relative);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, contents).unwrap();
}

#[test]
fn sync_report_summarizes_a_full_incremental_pass() {
    let root = tempdir().unwrap();
    write(&root, "same.md", "same");
    write(&root, "updated.md", "old");
    write(&root, "removed.md", "old");
    let config = SyncConfig::new(root.path());
    let (first_snapshot, first_plan) = build_plan(&config, &okf_sync::Snapshot::default()).unwrap();
    assert_eq!(SyncReport::from(&first_plan).added, 3);

    write(&root, "updated.md", "new");
    write(&root, "added.md", "new");
    fs::remove_file(root.path().join("removed.md")).unwrap();
    let (_, second_plan) = build_plan(&config, &first_snapshot).unwrap();
    let report = SyncReport::from(&second_plan);

    assert_eq!(
        report,
        SyncReport {
            added: 1,
            updated: 1,
            removed: 1,
            unchanged: 1,
        }
    );
    assert_eq!(report.total(), 4);
}

#[test]
fn file_hash_matches_memory_hash_and_changes_with_content() {
    let directory = tempdir().unwrap();
    let path = directory.path().join("concept.md");
    fs::write(&path, b"one").unwrap();

    let first = hash_file(&path).unwrap();
    fs::write(&path, b"two").unwrap();
    let second = hash_file(&path).unwrap();

    assert_eq!(first, hash_bytes(b"one"));
    assert_ne!(first, second);
    assert_eq!(first.len(), 64);
}

#[cfg(unix)]
#[test]
fn rejects_a_candidate_markdown_symlink_that_escapes_the_bundle() {
    use std::os::unix::fs::symlink;

    let bundle = tempdir().unwrap();
    let outside = tempdir().unwrap();
    fs::write(outside.path().join("outside.md"), "outside").unwrap();
    symlink(
        outside.path().join("outside.md"),
        bundle.path().join("escape.md"),
    )
    .unwrap();
    let config = SyncConfig::new(bundle.path());

    let result = discover(&config);

    assert!(matches!(
        result,
        Err(SyncError::SymlinkEscape { path, root })
            if path == bundle.path().join("escape.md") && root == bundle.path()
    ));
}

#[cfg(unix)]
#[test]
fn symlinks_resolving_inside_the_bundle_are_skipped() {
    use std::os::unix::fs::symlink;

    let bundle = tempdir().unwrap();
    write(&bundle, "docs/original.md", "content");
    symlink(
        bundle.path().join("docs/original.md"),
        bundle.path().join("alias.md"),
    )
    .unwrap();
    let config = SyncConfig::new(bundle.path());

    let snapshot = discover(&config).unwrap();

    assert_eq!(snapshot.len(), 1);
    assert!(snapshot.get("docs/original.md").is_some());
    assert!(snapshot.get("alias.md").is_none());
}

#[cfg(unix)]
#[test]
fn excluded_symlinks_are_skipped_without_being_resolved() {
    use std::os::unix::fs::symlink;

    let bundle = tempdir().unwrap();
    let outside = tempdir().unwrap();
    fs::write(outside.path().join("outside.md"), "outside").unwrap();
    symlink(outside.path(), bundle.path().join("escape")).unwrap();
    let config = SyncConfig::new(bundle.path()).with_exclude(["escape"]);

    let snapshot = discover(&config).unwrap();

    assert!(snapshot.is_empty());
}

#[cfg(unix)]
#[test]
fn a_dangling_candidate_markdown_symlink_is_ignored() {
    use std::os::unix::fs::symlink;

    let bundle = tempdir().unwrap();
    write(&bundle, "real.md", "content");
    symlink(
        bundle.path().join("missing-target.md"),
        bundle.path().join("broken.md"),
    )
    .unwrap();
    let config = SyncConfig::new(bundle.path());

    let snapshot = discover(&config).unwrap();

    assert_eq!(snapshot.len(), 1);
    assert!(snapshot.get("real.md").is_some());
    assert!(snapshot.get("broken.md").is_none());
}

#[cfg(unix)]
#[test]
fn a_dangling_non_markdown_symlink_is_ignored_and_real_documents_are_indexed() {
    use std::os::unix::fs::symlink;

    let bundle = tempdir().unwrap();
    write(&bundle, "real.md", "content");
    symlink(
        bundle.path().join("missing-target.txt"),
        bundle.path().join("dangling.txt"),
    )
    .unwrap();
    let config = SyncConfig::new(bundle.path());

    let snapshot = discover(&config).unwrap();

    assert_eq!(snapshot.len(), 1);
    assert!(snapshot.get("real.md").is_some());
}

#[cfg(unix)]
#[test]
fn an_irrelevant_symlink_to_an_outside_directory_does_not_abort_discovery() {
    use std::os::unix::fs::symlink;

    let bundle = tempdir().unwrap();
    let outside = tempdir().unwrap();
    write(&bundle, "real.md", "content");
    symlink(outside.path(), bundle.path().join("vendor")).unwrap();
    let config = SyncConfig::new(bundle.path());

    let snapshot = discover(&config).unwrap();

    assert_eq!(snapshot.len(), 1);
    assert!(snapshot.get("real.md").is_some());
}

#[test]
fn oversized_files_are_reported_with_their_size_and_the_limit() {
    let bundle = tempdir().unwrap();
    write(&bundle, "huge.md", "0123456789");
    let config = SyncConfig::new(bundle.path()).with_max_file_bytes(9);

    let result = build_plan(&config, &okf_sync::Snapshot::default());

    match result {
        Err(SyncError::FileTooLarge {
            path,
            size_bytes,
            limit_bytes,
        }) => {
            assert_eq!(path, bundle.path().join("huge.md"));
            assert_eq!(size_bytes, 10);
            assert_eq!(limit_bytes, 9);
        }
        other => panic!("expected FileTooLarge, got {other:?}"),
    }
}

#[test]
fn overpopulated_bundles_are_reported_with_the_count_and_the_limit() {
    let bundle = tempdir().unwrap();
    write(&bundle, "one.md", "1");
    write(&bundle, "two.md", "2");
    let config = SyncConfig::new(bundle.path()).with_max_files(1);

    let result = build_plan(&config, &okf_sync::Snapshot::default());

    assert!(matches!(
        result,
        Err(SyncError::TooManyFiles { count: 2, limit: 1 })
    ));
}
