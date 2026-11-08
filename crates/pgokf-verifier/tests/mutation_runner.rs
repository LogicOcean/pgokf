use pgokf_verifier::mutations::{Campaign, Mutation, Target};
use std::{fs, path::Path, process::Command};
fn campaign() -> Campaign {
    Campaign {
        mutations: vec![Mutation {
            name: "probe".into(),
            path: "crates/pgokf-verifier/src/probe.rs".into(),
            before: "original".into(),
            after: "changed".into(),
            test: "probe".into(),
            target: Target::Lib,
            failure_contains: "assertion failed".into(),
        }],
    }
}
fn git(root: &Path, args: &[&str]) {
    assert!(
        Command::new("git")
            .arg("-C")
            .arg(root)
            .args(args)
            .output()
            .unwrap()
            .status
            .success()
    );
}
fn cleaned(evidence: &Path) {
    let value: serde_json::Value =
        serde_json::from_slice(&fs::read(evidence.join("cleanup.json")).unwrap()).unwrap();
    assert_eq!(value["owned_directory_removed"], true);
    let ownership: serde_json::Value =
        serde_json::from_slice(&fs::read(evidence.join("ownership.json")).unwrap()).unwrap();
    assert!(!Path::new(ownership["owned_directory"].as_str().unwrap()).exists());
}
#[test]
fn failed_clone_still_records_owned_cleanup_and_no_kill() {
    let root = tempfile::tempdir().unwrap();
    let reports = tempfile::tempdir().unwrap();
    let out = reports.path().join("case");
    assert!(pgokf_verifier::mutations::run(root.path(), &campaign(), &out).is_err());
    cleaned(&out);
    assert!(!out.join("results.json").exists());
}
#[test]
fn traversal_is_rejected_before_any_output() {
    let root = tempfile::tempdir().unwrap();
    let reports = tempfile::tempdir().unwrap();
    let out = reports.path().join("case");
    let mut spec = campaign();
    spec.mutations[0].path = "../foreign.rs".into();
    assert!(pgokf_verifier::mutations::run(root.path(), &spec, &out).is_err());
    assert!(!out.exists());
}
#[cfg(unix)]
#[test]
fn cloned_symlink_cannot_mutate_an_external_file() {
    let root = tempfile::tempdir().unwrap();
    let foreign = tempfile::tempdir().unwrap();
    let reports = tempfile::tempdir().unwrap();
    let sentinel = foreign.path().join("foreign.rs");
    fs::write(&sentinel, "original").unwrap();
    let source = root.path().join("crates/pgokf-verifier/src");
    fs::create_dir_all(&source).unwrap();
    std::os::unix::fs::symlink(&sentinel, source.join("probe.rs")).unwrap();
    git(root.path(), &["init", "--quiet"]);
    git(root.path(), &["add", "."]);
    git(
        root.path(),
        &[
            "-c",
            "user.name=Verifier",
            "-c",
            "user.email=verifier@example.invalid",
            "commit",
            "--quiet",
            "-m",
            "fixture",
        ],
    );
    let out = reports.path().join("case");
    assert!(
        pgokf_verifier::mutations::run(root.path(), &campaign(), &out)
            .unwrap_err()
            .contains("owned regular file")
    );
    assert_eq!(fs::read_to_string(sentinel).unwrap(), "original");
    cleaned(&out);
}
