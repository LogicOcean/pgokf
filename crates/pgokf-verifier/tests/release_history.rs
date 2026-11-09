use pgokf_verifier::{process::CommandSpec, release_runtime::*};
use std::path::{Path, PathBuf};
fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}
fn git(root: &Path, args: &[&str]) -> String {
    let mut spec = CommandSpec::new("git");
    spec.cwd = Some(root.into());
    spec.args = args.iter().map(Into::into).collect();
    String::from_utf8(spec.checked().unwrap().stdout)
        .unwrap()
        .trim()
        .into()
}
#[test]
fn real_history_absent_moved_and_retired_sources_fail() {
    history_check(&root()).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let shallow = dir.path().join("shallow");
    std::fs::create_dir(&shallow).unwrap();
    git(&shallow, &["init", "-q"]);
    let head = git(&root(), &["rev-parse", "HEAD"]);
    git(
        &shallow,
        &[
            "fetch",
            "--quiet",
            "--depth",
            "1",
            "--no-tags",
            &format!("file://{}", root().canonicalize().unwrap().display()),
            &head,
        ],
    );
    assert!(
        history_check(&shallow)
            .unwrap_err()
            .contains("release history absent")
    );
    let clone = dir.path().join("clone");
    git(
        dir.path(),
        &[
            "clone",
            "--quiet",
            "--shared",
            "--no-checkout",
            &root().to_string_lossy(),
            &clone.to_string_lossy(),
        ],
    );
    history_check(&clone).unwrap();
    for (tag, _, commit) in HISTORY {
        git(&clone, &["checkout", "--quiet", "--detach", commit]);
        assert!(
            pgokf_verifier::release::compatibility_source(&clone).is_err(),
            "{tag}"
        );
        for (event, input) in [
            (format!("refs/tags/{tag}"), String::new()),
            ("refs/heads/main".into(), tag.into()),
            ("refs/heads/main".into(), String::new()),
        ] {
            assert!(resolve(&clone, "workflow_dispatch", &event, &input).is_err());
        }
    }
    git(&clone, &["tag", "--force", "v0.2.0", HISTORY[1].1]);
    assert!(
        history_check(&clone)
            .unwrap_err()
            .contains("immutable release tag v0.2.0 moved")
    );
}
#[test]
fn real_git_release_dispatch_and_transactional_refusal() {
    let dir = tempfile::tempdir().unwrap();
    for path in [
        "Cargo.toml",
        "Cargo.lock",
        "META.json",
        "crates/extension/pgokf.control",
    ] {
        let dest = dir.path().join(path);
        std::fs::create_dir_all(dest.parent().unwrap()).unwrap();
        std::fs::copy(root().join(path), dest).unwrap();
    }
    for entry in std::fs::read_dir(root().join("crates")).unwrap() {
        let path = entry.unwrap().path().join("Cargo.toml");
        if path.is_file() {
            let dest = dir.path().join(path.strip_prefix(root()).unwrap());
            std::fs::create_dir_all(dest.parent().unwrap()).unwrap();
            std::fs::copy(path, dest).unwrap();
        }
    }
    git(dir.path(), &["init", "-q"]);
    git(
        dir.path(),
        &["config", "user.email", "fixture@example.invalid"],
    );
    git(dir.path(), &["config", "user.name", "Release fixture"]);
    git(dir.path(), &["add", "."]);
    git(dir.path(), &["commit", "-qm", "Current source"]);
    let version = env!("CARGO_PKG_VERSION");
    let tag = format!("v{version}");
    git(dir.path(), &["tag", "-a", &tag, "-m", "Annotated fixture"]);
    let branch = resolve(dir.path(), "push", "refs/heads/main", "").unwrap();
    assert!(!branch.publish);
    assert!(
        !resolve(dir.path(), "push", &format!("refs/tags/{tag}"), "")
            .unwrap()
            .publish
    );
    assert!(
        resolve(dir.path(), "workflow_dispatch", "refs/heads/main", &tag)
            .unwrap()
            .publish
    );
    assert!(resolve(dir.path(), "workflow_dispatch", "refs/heads/main", "v0.2.0").is_err());
    let output = dir.path().join("output");
    write_intent(&output, &branch).unwrap();
    let previous = std::fs::read(&output).unwrap();
    std::fs::write(dir.path().join("META.json"), "{}").unwrap();
    let refused = resolve(dir.path(), "workflow_dispatch", "refs/heads/main", &tag);
    assert!(refused.is_err());
    assert_eq!(std::fs::read(output).unwrap(), previous);
    git(dir.path(), &["checkout", "--", "META.json"]);
    std::fs::write(dir.path().join("other"), "second commit").unwrap();
    git(dir.path(), &["add", "other"]);
    git(dir.path(), &["commit", "-qm", "New branch source"]);
    assert!(
        resolve(dir.path(), "workflow_dispatch", "refs/heads/main", &tag)
            .unwrap_err()
            .contains("HEAD is not")
    );
}

#[test]
fn typed_rollback_contract_rejects_mutable_or_incompatible_images() {
    let source = pgokf_verifier::release_identity::source_identity(&root()).unwrap();
    let image = ImageManifest {
        source,
        source_commit: "a".repeat(40),
        image_id: format!("sha256:{}", "b".repeat(64)),
        postgres_major: 18,
        postgres_image_tag: "18".into(),
        tooling_commit: "c".repeat(40),
    };
    let formula = pgokf_verifier::release_identity::formula_tuple(
        &std::fs::read_to_string(root().join("packaging/homebrew/pgokf.rb")).unwrap(),
    )
    .unwrap();
    let contract = RollbackManifest {
        schema_version: 1,
        from: image.clone(),
        to: image,
        previous_formula: formula,
    };
    let mut contract = contract;
    contract.to.source.version = "0.2.0".into();
    contract.to.source.distribution_version = "0.2.0".into();
    validate_rollback(&contract).unwrap();
    let mut bad = contract.clone();
    bad.to.image_id = "pgokf:latest".into();
    assert!(validate_rollback(&bad).is_err());
    let mut bad = contract.clone();
    bad.to.source_commit = "main".into();
    assert!(validate_rollback(&bad).is_err());
    let mut bad = contract.clone();
    bad.to.postgres_major = 19;
    assert!(validate_rollback(&bad).is_err());
    let mut bad = contract.clone();
    bad.schema_version = 2;
    assert!(validate_rollback(&bad).is_err());
    let mut bad = contract;
    bad.previous_formula.sha256 = "d".repeat(64);
    assert!(validate_rollback(&bad).is_err());
}

#[test]
fn rollback_rejects_correlated_source_and_provider_forgery() {
    let source = pgokf_verifier::release_identity::source_identity(&root()).unwrap();
    let image = ImageManifest {
        source,
        source_commit: "a".repeat(40),
        image_id: format!("sha256:{}", "b".repeat(64)),
        postgres_major: 18,
        postgres_image_tag: "18".into(),
        tooling_commit: "c".repeat(40),
    };
    let formula = pgokf_verifier::release_identity::formula_tuple(
        &std::fs::read_to_string(root().join("packaging/homebrew/pgokf.rb")).unwrap(),
    )
    .unwrap();
    let mut contract = RollbackManifest {
        schema_version: 1,
        from: image.clone(),
        to: image,
        previous_formula: formula,
    };
    contract.to.source.version = "0.2.0".into();
    contract.to.source.distribution_version = "0.2.0".into();
    validate_rollback(&contract).unwrap();
    for field in [
        "empty-packages",
        "distribution",
        "source-version",
        "provider-tag",
        "provider-major",
        "formula-target",
    ] {
        let mut bad = contract.clone();
        match field {
            "empty-packages" => {
                bad.from.source.packages.clear();
                bad.to.source.packages.clear();
            }
            "distribution" => {
                bad.from.source.distribution_version = "0.0.0".into();
                bad.to.source.distribution_version = "0.0.0".into();
            }
            "source-version" => {
                bad.from.source.version.clear();
                bad.to.source.version.clear();
            }
            "provider-tag" => {
                bad.from.postgres_image_tag = "19beta4".into();
                bad.to.postgres_image_tag = "19beta4".into();
            }
            "provider-major" => {
                bad.from.postgres_major = 20;
                bad.to.postgres_major = 20;
            }
            _ => {
                bad.to.source.version = "0.3.1".into();
                bad.to.source.distribution_version = "0.3.1".into();
            }
        }
        assert!(validate_rollback(&bad).is_err(), "{field}");
    }
}
