use std::fs;

use okf_sync::{
    DiscoverOptions, Error, FileState, SyncReport, discover, hash_bytes, hash_file, plan_sync,
};
use tempfile::tempdir;

fn state(path: &str, content: &[u8]) -> FileState {
    FileState::new(path, hash_bytes(content))
}

#[test]
fn discovers_markdown_with_defaults_in_stable_order() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    let files = discover(&root, &DiscoverOptions::default()).unwrap();
    let paths: Vec<_> = files
        .iter()
        .map(|file| file.relative_path.as_str())
        .collect();
    assert_eq!(paths, ["nested/child.md", "root.md"]);
}

#[test]
fn custom_include_and_exclude_globs_are_applied() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    let options = DiscoverOptions {
        include: vec!["nested/**".into(), "root.md".into()],
        exclude: vec!["**/child.md".into()],
        follow_symlinks: false,
    };
    let files = discover(&root, &options).unwrap();
    assert_eq!(files.len(), 1);
    assert_eq!(files[0].relative_path, "root.md");
}

#[test]
fn invalid_root_and_glob_are_actionable_errors() {
    let missing = std::path::Path::new("definitely-not-a-bundle");
    assert!(matches!(
        discover(missing, &DiscoverOptions::default()),
        Err(Error::InvalidRoot(_))
    ));
    let options = DiscoverOptions {
        include: vec!["[".into()],
        ..DiscoverOptions::default()
    };
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    assert!(matches!(
        discover(root, &options),
        Err(Error::InvalidGlob { .. })
    ));
}

#[test]
fn file_hash_matches_memory_hash_and_changes_with_content() {
    let directory = tempdir().unwrap();
    let path = directory.path().join("concept.md");
    fs::write(&path, b"one").unwrap();
    assert_eq!(hash_file(&path).unwrap(), hash_bytes(b"one"));
    let first = hash_file(&path).unwrap();
    fs::write(&path, b"two").unwrap();
    assert_ne!(first, hash_file(&path).unwrap());
    assert_eq!(first.as_str().len(), 64);
}

#[test]
fn plans_added_updated_removed_and_unchanged_files() {
    let previous = vec![
        state("removed.md", b"old"),
        state("updated.md", b"old"),
        state("same.md", b"same"),
    ];
    let current = vec![
        state("same.md", b"same"),
        state("updated.md", b"new"),
        state("added.md", b"new"),
    ];
    let plan = plan_sync(&previous, &current);
    assert_eq!(plan.added, ["added.md"]);
    assert_eq!(plan.updated, ["updated.md"]);
    assert_eq!(plan.removed, ["removed.md"]);
    assert_eq!(plan.unchanged, ["same.md"]);
    assert!(!plan.is_empty());
    assert_eq!(
        SyncReport::from(&plan),
        SyncReport {
            added: 1,
            updated: 1,
            removed: 1,
            unchanged: 1,
        }
    );
    assert_eq!(SyncReport::from(&plan).total(), 4);
}

#[test]
fn identical_snapshots_produce_no_mutations() {
    let snapshot = vec![state("b.md", b"b"), state("a.md", b"a")];
    let plan = plan_sync(&snapshot, &snapshot);
    assert!(plan.is_empty());
    assert_eq!(plan.unchanged, ["a.md", "b.md"]);
}

#[test]
fn empty_snapshots_handle_initial_and_full_removal_syncs() {
    let current = vec![state("a.md", b"a")];
    assert_eq!(plan_sync(&[], &current).added, ["a.md"]);
    assert_eq!(plan_sync(&current, &[]).removed, ["a.md"]);
}

#[cfg(unix)]
#[test]
fn rejects_followed_symlinks_that_escape_the_bundle() {
    use std::os::unix::fs::symlink;

    let bundle = tempdir().unwrap();
    let outside = tempdir().unwrap();
    fs::write(outside.path().join("outside.md"), "outside").unwrap();
    symlink(outside.path(), bundle.path().join("escape")).unwrap();
    let options = DiscoverOptions {
        follow_symlinks: true,
        ..DiscoverOptions::default()
    };
    assert!(matches!(
        discover(bundle.path(), &options),
        Err(Error::SymlinkEscape(_))
    ));
}
