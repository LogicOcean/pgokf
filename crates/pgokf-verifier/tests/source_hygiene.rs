use pgokf_verifier::source;
use sha2::{Digest, Sha256};
use std::{collections::BTreeMap, fs, process::Command};
fn check(root: &std::path::Path) -> Result<(), String> {
    let bytes = fs::read(root.join("source-review.json")).map_err(|e| e.to_string())?;
    let trusted = pgokf_verifier::json::from_slice(&bytes)?;
    source::check_reviewed(root, &trusted)
}
fn fixture() -> tempfile::TempDir {
    let root = tempfile::tempdir().unwrap();
    assert!(
        Command::new("git")
            .arg("init")
            .arg("--quiet")
            .arg(root.path())
            .status()
            .unwrap()
            .success()
    );
    root
}
#[allow(clippy::format_collect)]
fn review(root: &std::path::Path, files: &[&str]) {
    let files: BTreeMap<_, _> = files
        .iter()
        .map(|name| {
            (
                *name,
                Sha256::digest(fs::read(root.join(name)).unwrap())
                    .iter()
                    .map(|b| format!("{b:02x}"))
                    .collect::<String>(),
            )
        })
        .collect();
    fs::write(
        root.join("source-review.json"),
        serde_json::to_vec(&serde_json::json!({"version":1,"files":files.into_iter().map(|(name,sha256)| (name,serde_json::json!({"sha256":sha256,"executable":false}))).collect::<BTreeMap<_,_>>()})).unwrap(),
    )
    .unwrap();
}
#[test]
fn computed_and_transitive_execution_requires_review() {
    let root = fixture();
    fs::write(
        root.path().join("gate.sh"),
        "#!/bin/sh\ncargo test --locked\n",
    )
    .unwrap();
    review(root.path(), &["gate.sh"]);
    assert!(check(root.path()).is_ok());
    for command in [
        "p=py; q=thon; $p$q -c pass",
        "sh payload.data",
        "eval $(cat payload.data)",
        "$(printf '\\160\\171\\164\\150\\157\\156') -c pass",
    ] {
        fs::write(root.path().join("gate.sh"), command).unwrap();
        assert!(check(root.path()).is_err(), "{command}");
    }
}
#[test]
fn new_data_wrapper_and_dependency_changes_fail_closed() {
    let root = fixture();
    fs::write(root.path().join("gate.sh"), "cargo test").unwrap();
    review(root.path(), &["gate.sh"]);
    fs::write(root.path().join("payload.data"), "p=py; q=thon; $p$q").unwrap();
    assert!(check(root.path()).is_err());
}
#[test]
fn docs_alias_cannot_launder_exemption() {
    let root = fixture();
    fs::create_dir_all(root.path().join(".github/workflows")).unwrap();
    let name = ".github/workflows/docs.yml";
    fs::write(root.path().join(name), "jobs:\n  docs:\n    steps:\n    - name: Install MkDocs Material\n      run: pip install mkdocs-material\n      env: &injected {TOOL: python3}\n    - name: Install MkDocs Material\n      run: pip install mkdocs-material\n      <<: {env: *injected}\n").unwrap();
    review(root.path(), &[name]);
    assert!(check(root.path()).is_err());
}
#[test]
fn exact_docs_steps_are_allowed_and_changed_steps_refused() {
    let root = fixture();
    fs::create_dir_all(root.path().join(".github/workflows")).unwrap();
    let name = ".github/workflows/docs.yml";
    for (extra, ok) in [("", true), (" pytest", false)] {
        fs::write(root.path().join(name), format!("jobs:\n  docs:\n    steps:\n    - name: Install MkDocs Material\n      run: pip install mkdocs-material{extra}\n")).unwrap();
        review(root.path(), &[name]);
        assert_eq!(check(root.path()).is_ok(), ok);
    }
}
#[test]
fn duplicate_review_keys_and_stale_entries_are_refused() {
    let root = fixture();
    fs::write(
        root.path().join("source-review.json"),
        "{\"version\":1,\"version\":1,\"files\":{}}",
    )
    .unwrap();
    assert!(check(root.path()).is_err());
    fs::write(
        root.path().join("source-review.json"),
        "{\"version\":1,\"files\":{\"missing\":\"digest\"}}",
    )
    .unwrap();
    assert!(check(root.path()).is_err());
}
#[test]
fn rehashing_hostile_commands_does_not_change_trusted_review() {
    let root = fixture();
    fs::write(root.path().join("gate.sh"), "cargo test --locked").unwrap();
    review(root.path(), &["gate.sh"]);
    let trusted = pgokf_verifier::json::from_slice(
        &fs::read(root.path().join("source-review.json")).unwrap(),
    )
    .unwrap();
    for command in [
        "p'y'thon3 -c pass",
        "p=py; q=thon; $p$q -c pass",
        "eval $(base64 -d payload.data)",
        "sh extensionless",
    ] {
        fs::write(root.path().join("gate.sh"), command).unwrap();
        review(root.path(), &["gate.sh"]);
        assert!(
            source::check_reviewed(root.path(), &trusted)
                .unwrap_err()
                .contains("differs from trusted verifier inventory")
        );
    }
}
#[test]
fn reviewed_python_files_shebangs_dependencies_and_local_actions_still_fail() {
    for (name, text) in [
        ("hidden.PY", "pass"),
        ("hidden.pyw", "pass"),
        ("hidden.pyc", "bytes"),
        ("hidden.rs", "#!/usr/bin/env python3\npass"),
        ("requirements.txt", "PyYAML==6.0.3"),
        ("tools/requirements-dev.txt", "PyYAML==6.0.3"),
        ("Cargo.toml", "[dependencies]\npytest = '1'"),
        ("Dockerfile.test", "RUN python3 -m unittest"),
        (
            "action.yml",
            "runs:\n  using: composite\n  steps:\n  - run: python3 check\n",
        ),
        ("Makefile", "test:\n\tpytest"),
    ] {
        let root = fixture();
        fs::create_dir_all(root.path().join(name).parent().unwrap()).unwrap();
        fs::write(root.path().join(name), text).unwrap();
        review(root.path(), &[name]);
        assert!(check(root.path()).is_err(), "{name}");
    }
}
#[cfg(unix)]
#[test]
fn execution_permission_changes_require_review() {
    use std::os::unix::fs::PermissionsExt;
    let root = fixture();
    let path = root.path().join("payload.data");
    fs::write(&path, "exit 0").unwrap();
    review(root.path(), &["payload.data"]);
    assert!(check(root.path()).is_ok());
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
    assert!(check(root.path()).is_err());
}
#[test]
fn review_version_and_unknown_fields_are_closed() {
    let root = fixture();
    for manifest in [
        r#"{"version":2,"files":{}}"#,
        r#"{"version":1,"files":{},"unexpected":true}"#,
    ] {
        fs::write(root.path().join("source-review.json"), manifest).unwrap();
        assert!(check(root.path()).is_err(), "{manifest}");
    }
    fs::write(root.path().join("gate.sh"), "cargo test").unwrap();
    review(root.path(), &["gate.sh"]);
    let mut manifest: serde_json::Value =
        serde_json::from_slice(&fs::read(root.path().join("source-review.json")).unwrap()).unwrap();
    manifest["files"]["gate.sh"]["unexpected"] = serde_json::json!(true);
    fs::write(
        root.path().join("source-review.json"),
        serde_json::to_vec(&manifest).unwrap(),
    )
    .unwrap();
    assert!(check(root.path()).is_err());
}
#[test]
fn stale_review_entry_cannot_authorize_an_absent_input() {
    let root = fixture();
    fs::write(root.path().join("gate.sh"), "cargo test").unwrap();
    review(root.path(), &["gate.sh"]);
    fs::remove_file(root.path().join("gate.sh")).unwrap();
    assert!(
        check(root.path())
            .unwrap_err()
            .contains("stale source review entry")
    );
}
#[test]
fn workflow_errors_are_deterministically_sorted() {
    let root = fixture();
    let name = ".github/workflows/ci.yml";
    fs::create_dir_all(root.path().join(".github/workflows")).unwrap();
    fs::write(
        root.path().join(name),
        "steps:\n  - run: python3 z\n  - run: python3 a\n",
    )
    .unwrap();
    review(root.path(), &[name]);
    let error = check(root.path()).unwrap_err();
    let lines: Vec<_> = error.lines().collect();
    let mut sorted = lines.clone();
    sorted.sort_unstable();
    assert_eq!(lines, sorted);
}
