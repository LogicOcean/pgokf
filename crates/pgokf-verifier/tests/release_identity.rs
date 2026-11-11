use pgokf_verifier::release_identity::*;
use std::path::{Path, PathBuf};
fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}
fn fixture() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    for name in [
        "Cargo.toml",
        "Cargo.lock",
        "META.json",
        "crates/extension/pgokf.control",
    ] {
        let dest = dir.path().join(name);
        std::fs::create_dir_all(dest.parent().unwrap()).unwrap();
        std::fs::copy(root().join(name), dest).unwrap();
    }
    for entry in std::fs::read_dir(root().join("crates")).unwrap() {
        let source = entry.unwrap().path().join("Cargo.toml");
        if source.is_file() {
            let dest = dir.path().join(source.strip_prefix(root()).unwrap());
            std::fs::create_dir_all(dest.parent().unwrap()).unwrap();
            std::fs::copy(source, dest).unwrap();
        }
    }
    dir
}
fn edit(root: &Path, path: &str, before: &str, after: &str) {
    let path = root.join(path);
    let source = std::fs::read_to_string(&path).unwrap();
    assert!(source.contains(before));
    std::fs::write(path, source.replace(before, after)).unwrap();
}
#[test]
fn release_identity_includes_verifier_and_all_distribution_artifacts() {
    let identity = source_identity(&root()).unwrap();
    assert!(identity.packages.contains("pgokf-verifier"));
    distribution_identity(&root(), &identity).unwrap();
}
#[test]
fn stable_0_5_0_source_and_distribution_identity_are_exact() {
    let identity = source_identity(&root()).unwrap();
    assert_eq!(identity.version, "0.5.0");
    assert_eq!(identity.distribution_version, "0.5.0");
    distribution_identity(&root(), &identity).unwrap();
}
#[test]
fn release_identity_mutations_fail() {
    for (path, before, after) in [
        ("Cargo.toml", "version = \"0.5.0\"", "version = \"0.2.0\""),
        ("Cargo.lock", "version = \"0.5.0\"", "version = \"0.2.0\""),
        (
            "META.json",
            "\"version\": \"0.5.0\"",
            "\"version\": \"0.2.0\"",
        ),
        ("crates/extension/Cargo.toml", "=0.5.0", "=0.2.0"),
        ("crates/extension/pgokf.control", "0.5.0", "0.3.0"),
    ] {
        let f = fixture();
        edit(f.path(), path, before, after);
        assert!(source_identity(f.path()).is_err(), "{path}");
    }
}
#[test]
fn duplicate_lock_meta_and_control_identities_fail() {
    for (path, added) in [
        (
            "Cargo.lock",
            "\n[[package]]\nname = \"pgokf-verifier\"\nversion = \"0.5.0\"\n",
        ),
        (
            "crates/extension/pgokf.control",
            "\ndefault_version = '0.5.0'\n",
        ),
    ] {
        let f = fixture();
        let file = f.path().join(path);
        let mut body = std::fs::read_to_string(&file).unwrap();
        body.push_str(added);
        std::fs::write(file, body).unwrap();
        assert!(source_identity(f.path()).is_err());
    }
    let f = fixture();
    edit(
        f.path(),
        "META.json",
        "\"version\": \"0.5.0\"",
        "\"version\": \"0.5.0\", \"version\": \"0.5.0\"",
    );
    assert!(source_identity(f.path()).is_err());
}
#[test]
fn target_internal_pins_and_vulnerable_dependency_are_checked() {
    let f = fixture();
    let file = f.path().join("crates/pgokf-verifier/Cargo.toml");
    let mut text = std::fs::read_to_string(&file).unwrap();
    text.push_str("\n[target.'cfg(windows)'.dependencies]\npgokf-web = { path = '../pgokf-web', version = '0.4' }\n");
    std::fs::write(file, text).unwrap();
    assert!(source_identity(f.path()).unwrap_err().contains("exact"));
    let f = fixture();
    let file = f.path().join("Cargo.lock");
    let mut text = std::fs::read_to_string(&file).unwrap();
    text.push_str("\n[[package]]\nname = 'serde_cbor'\nversion = '0.11'\n");
    std::fs::write(file, text).unwrap();
    assert!(
        source_identity(f.path())
            .unwrap_err()
            .contains("serde_cbor")
    );
}
#[test]
fn formula_authenticated_forward_and_rollback_tuples() {
    let source = std::fs::read_to_string(root().join("packaging/homebrew/pgokf.rb")).unwrap();
    // Pre-tag, the formula keeps consuming the last authenticated release: a
    // formula cannot carry the digest of the archive that contains it. The
    // crate's own version authenticates only once its digest is recorded, and
    // the published 0.2.0 tuple remains the authenticated rollback target.
    let tuple = formula_tuple(&source).unwrap();
    assert_ne!(tuple.version, env!("CARGO_PKG_VERSION"));
    let mut records = published_digests();
    authenticate_formula(&tuple, &records).unwrap();
    let rollback = FormulaTuple {
        version: "0.2.0".into(),
        sha256: PUBLISHED_DIGEST.into(),
        assertions: vec!["0.2.0".into(); 2],
    };
    authenticate_formula(&rollback, &records).unwrap();
    let mut current = tuple.clone();
    current.version = env!("CARGO_PKG_VERSION").into();
    current.assertions = vec![current.version.clone(); 2];
    current.sha256 = "a".repeat(64);
    assert!(authenticate_formula(&current, &records).is_err());
    records.insert(current.version.clone(), current.sha256.clone());
    authenticate_formula(&current, &records).unwrap();
    authenticate_formula(&tuple, &records).unwrap();
    for bad in [
        FormulaTuple {
            sha256: tuple.sha256.clone(),
            ..current.clone()
        },
        FormulaTuple {
            assertions: vec![tuple.version.clone(), current.version.clone()],
            ..current.clone()
        },
        FormulaTuple {
            assertions: vec![],
            ..current.clone()
        },
    ] {
        assert!(authenticate_formula(&bad, &records).is_err());
    }
    records.insert("0.3.0".into(), "b".repeat(64));
    assert!(
        authenticate_formula(
            &FormulaTuple {
                version: "0.3.0".into(),
                sha256: "b".repeat(64),
                assertions: vec!["0.3.0".into(); 2]
            },
            &records
        )
        .is_err()
    );
    assert!(formula_tuple(&(source.clone() + "\nurl \"bad\"\n")).is_err());
}
#[test]
fn formula_observation_expectations_are_independent_and_unique() {
    let source = std::fs::read_to_string(root().join("packaging/homebrew/pgokf.rb")).unwrap();
    let version = formula_tuple(&source).unwrap().version;
    for field in ["expected_control_version", "expected_extension_version"] {
        let declaration = format!("{field} = \"{version}\"");
        assert!(formula_tuple(&source.replace(&declaration, "")).is_err());
        assert!(formula_tuple(&format!("{source}\n{declaration}\n")).is_err());
        let changed =
            formula_tuple(&source.replace(&declaration, &format!("{field} = \"0.1.0\""))).unwrap();
        assert!(authenticate_formula(&changed, &published_digests()).is_err());
    }
}
#[test]
fn publication_is_dispatch_only_and_requires_exact_clean_tag_commit() {
    let v = env!("CARGO_PKG_VERSION");
    let head = "a".repeat(40);
    let branch = release_intent(v, &head, true, "push", "refs/heads/main", "", |_| {
        panic!("branch needs no tag")
    })
    .unwrap();
    assert!(!branch.publish);
    let tag_push = release_intent(
        v,
        &head,
        true,
        "push",
        &format!("refs/tags/v{v}"),
        "",
        |_| panic!("tag pushes must not resolve publication tags"),
    )
    .unwrap();
    assert!(!tag_push.publish);
    assert!(
        release_intent(
            v,
            &head,
            true,
            "workflow_dispatch",
            "refs/heads/main",
            &format!("v{v}"),
            |_| Ok(head.clone())
        )
        .unwrap()
        .publish
    );
    assert!(
        release_intent(
            v,
            &head,
            true,
            "workflow_dispatch",
            "refs/heads/main",
            &format!("v{v}"),
            |_| Ok("b".repeat(40))
        )
        .is_err()
    );
    for (version, clean, event_name, event, input) in [
        (v, false, "push", "refs/heads/main", ""),
        (
            "0.2.0",
            true,
            "workflow_dispatch",
            "refs/heads/main",
            "v0.2.0",
        ),
        (v, true, "workflow_dispatch", "refs/heads/main", "v0.2.0"),
        (v, true, "workflow_dispatch", "refs/heads/main", "main"),
        (v, true, "push", "refs/heads/main", &format!("v{v}")),
    ] {
        assert!(
            release_intent(version, &head, clean, event_name, event, input, |_| Ok(
                head.clone()
            ))
            .is_err()
        );
    }
}

#[test]
fn formula_sql_retains_quoted_extension_identity() {
    let formula = std::fs::read_to_string(root().join("packaging/homebrew/pgokf.rb")).unwrap();
    let expression = formula
        .split_once("output = Utils.safe_popen_read(")
        .unwrap()
        .1
        .split_once("\n      )")
        .unwrap()
        .0
        .trim()
        .trim_end_matches(',');
    let code = format!(
        "require 'pathname'; require 'json'; pg_bin=Pathname.new('/bin with space'); port=15432; puts JSON.generate([{expression}].map(&:to_s))"
    );
    let mut spec = pgokf_verifier::process::CommandSpec::new("ruby");
    spec.args = vec!["-e".into(), code.into()];
    let observation: Vec<String> =
        pgokf_verifier::json::from_slice(&spec.checked().unwrap().stdout).unwrap();
    assert_eq!(
        observation,
        [
            "/bin with space/psql",
            "-h",
            "127.0.0.1",
            "-p",
            "15432",
            "-U",
            "postgres",
            "-d",
            "postgres",
            "-v",
            "ON_ERROR_STOP=1",
            "-tAc",
            "CREATE EXTENSION pgokf; SELECT extversion FROM pg_extension WHERE extname='pgokf';"
        ]
    );
}
#[test]
fn dependency_patch_and_distribution_feature_boundaries() {
    {
        let path = "vendor/pgrx/Cargo.toml";
        let source = std::fs::read_to_string(root().join(path)).unwrap();
        assert!(
            !source
                .lines()
                .any(|line| line.trim().starts_with("serde_cbor"))
        );
    }
    let docker = std::fs::read_to_string(root().join("packaging/docker/Dockerfile")).unwrap();
    assert!(docker.contains("ARG PG_IMAGE_TAG=${PG_MAJOR}"));
    assert_eq!(docker.matches("FROM ${PG_BASE_IMAGE}").count(), 2);
    assert!(docker.contains("-- postgres-check"));
    let export =
        std::fs::read_to_string(root().join("crates/extension/src/catalog/export.rs")).unwrap();
    assert!(export.contains("architecture-specific: `x86_64`"));
}

#[test]
fn workspace_dependency_pins_are_not_hidden_by_inheritance() {
    let f = fixture();
    edit(
        f.path(),
        "Cargo.toml",
        "[workspace.dependencies]",
        "[workspace.dependencies]\npgokf-web = { path = 'crates/pgokf-web', version = '0.4' }",
    );
    assert!(source_identity(f.path()).is_err());
}
#[test]
fn workspace_members_cannot_silently_drop_a_local_gate() {
    let f = fixture();
    edit(
        f.path(),
        "Cargo.toml",
        "    \"crates/pgokf-verifier\",\n",
        "",
    );
    assert!(source_identity(f.path()).is_err());
}
#[test]
fn dynamic_formula_declaration_suffix_is_refused() {
    let source = std::fs::read_to_string(root().join("packaging/homebrew/pgokf.rb")).unwrap();
    let archive = format!("v{}.tar.gz\"", formula_tuple(&source).unwrap().version);
    let bad = source.replace(&archive, &format!("{archive} + ENV.fetch(\"HOSTILE_URL\")"));
    assert!(formula_tuple(&bad).is_err());
}
#[test]
fn distribution_version_prefix_and_comment_cannot_hide_stale_value() {
    let f = fixture();
    for path in [
        "packaging/rpm/pgokf.spec",
        "deploy/compose/.env.example",
        ".github/ISSUE_TEMPLATE/bug_report.yml",
        "packaging/docker/Dockerfile",
        "packaging/docker/Dockerfile.companions",
        "crates/extension/sql/pgokf--0.5.0.sql",
    ] {
        let dest = f.path().join(path);
        std::fs::create_dir_all(dest.parent().unwrap()).unwrap();
        std::fs::copy(root().join(path), dest).unwrap();
    }
    let path = f.path().join("deploy/compose/.env.example");
    let text = std::fs::read_to_string(&path).unwrap();
    std::fs::write(path,format!("{}\n# ghcr.io/logicocean/pgokf:0.5.0-pg18\n# ghcr.io/logicocean/pgokf-companions:0.5.0\n",text.replace("0.5.0","0.5.1"))).unwrap();
    let identity = source_identity(f.path()).unwrap();
    assert!(distribution_identity(f.path(), &identity).is_err());
}

#[test]
fn resolved_dependency_patch_is_vendored_and_reaches_package_contexts() {
    fn scan(path: &Path) {
        for entry in std::fs::read_dir(path).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                scan(&path);
            } else if path.extension().is_some_and(|s| s == "rs") {
                let source = std::fs::read_to_string(&path).unwrap();
                assert!(
                    !source
                        .split(|c: char| !c.is_alphanumeric() && c != '_')
                        .any(|token| token == ["Postgres", "Type"].concat()),
                    "{}",
                    path.display()
                );
            }
        }
    }
    scan(&root().join("crates"));
    for file in [
        "packaging/docker/Dockerfile",
        "packaging/docker/Dockerfile.companions",
    ] {
        assert!(
            std::fs::read_to_string(root().join(file))
                .unwrap()
                .contains("COPY . .")
        );
    }
    assert!(
        !std::fs::read_to_string(root().join(".dockerignore"))
            .unwrap()
            .lines()
            .any(|s| s.trim_matches('/') == "vendor")
    );
    let mut spec = pgokf_verifier::process::CommandSpec::new("cargo");
    spec.cwd = Some(root());
    spec.args = ["metadata", "--locked", "--format-version", "1"]
        .into_iter()
        .map(Into::into)
        .collect();
    let metadata: serde_json::Value =
        pgokf_verifier::json::from_slice(&spec.checked().unwrap().stdout).unwrap();
    let packages = metadata["packages"].as_array().unwrap();
    let pgrx: Vec<_> = packages.iter().filter(|p| p["name"] == "pgrx").collect();
    assert_eq!(pgrx.len(), 1);
    assert_eq!(pgrx[0]["version"], "0.19.2");
    assert_eq!(
        Path::new(pgrx[0]["manifest_path"].as_str().unwrap())
            .canonicalize()
            .unwrap(),
        root()
            .join("vendor/pgrx/Cargo.toml")
            .canonicalize()
            .unwrap()
    );
    assert!(!packages.iter().any(|p| p["name"] == "serde_cbor"));
}

#[test]
fn active_distribution_declarations_cannot_be_replaced_by_comments() {
    for (file, before, after, comment) in [
        (
            "packaging/docker/Dockerfile",
            "ARG PGOKF_VERSION=\"0.5.0\"",
            "ARG PGOKF_VERSION=\"0.5.1\"",
            "# ARG PGOKF_VERSION=\"0.5.0\"",
        ),
        (
            ".github/ISSUE_TEMPLATE/bug_report.yml",
            "placeholder: \"0.5.0\"",
            "placeholder: \"0.5.1\"",
            "# \"0.5.0\"",
        ),
    ] {
        let f = fixture();
        for path in [
            "packaging/rpm/pgokf.spec",
            "deploy/compose/.env.example",
            ".github/ISSUE_TEMPLATE/bug_report.yml",
            "packaging/docker/Dockerfile",
            "packaging/docker/Dockerfile.companions",
            "crates/extension/sql/pgokf--0.5.0.sql",
        ] {
            let dest = f.path().join(path);
            std::fs::create_dir_all(dest.parent().unwrap()).unwrap();
            std::fs::copy(root().join(path), dest).unwrap();
        }
        let path = f.path().join(file);
        let source = std::fs::read_to_string(&path).unwrap();
        assert!(source.contains(before));
        std::fs::write(
            path,
            format!("{}\n{comment}\n", source.replace(before, after)),
        )
        .unwrap();
        assert!(
            distribution_identity(f.path(), &source_identity(f.path()).unwrap()).is_err(),
            "{file}"
        );
    }
}

#[test]
fn local_package_versions_and_duplicate_names_are_validated() {
    for (before, after) in [
        ("version.workspace = true", "version = \"0.2.0\""),
        ("name = \"pgokf-verifier\"", "name = \"pgokf-web\""),
    ] {
        let f = fixture();
        edit(f.path(), "crates/pgokf-verifier/Cargo.toml", before, after);
        assert!(source_identity(f.path()).is_err());
    }
}
#[test]
fn formula_retirement_assertions_and_digest_reuse_are_independent() {
    let source = std::fs::read_to_string(root().join("packaging/homebrew/pgokf.rb")).unwrap();
    let tuple = formula_tuple(&source).unwrap();
    let mut records = published_digests();
    records.insert("0.3.0".into(), "b".repeat(64));
    let retired = FormulaTuple {
        version: "0.3.0".into(),
        sha256: "b".repeat(64),
        assertions: vec!["0.3.0".into(); 2],
    };
    assert!(authenticate_formula(&retired, &records).is_err());
    let mut bad = tuple.clone();
    bad.assertions[0] = "0.1.0".into();
    assert!(authenticate_formula(&bad, &records).is_err());
    records.insert("0.3.1".into(), tuple.sha256.clone());
    let reused = FormulaTuple {
        version: "0.3.1".into(),
        assertions: vec!["0.3.1".into(); 2],
        ..tuple
    };
    assert!(authenticate_formula(&reused, &records).is_err());
}
#[test]
fn rpm_install_presence_and_documented_image_tags_are_validated() {
    for mutation in ["rpm", "install", "example"] {
        let f = fixture();
        for path in [
            "packaging/rpm/pgokf.spec",
            "deploy/compose/.env.example",
            ".github/ISSUE_TEMPLATE/bug_report.yml",
            "packaging/docker/Dockerfile",
            "packaging/docker/Dockerfile.companions",
            "crates/extension/sql/pgokf--0.5.0.sql",
        ] {
            let dest = f.path().join(path);
            std::fs::create_dir_all(dest.parent().unwrap()).unwrap();
            std::fs::copy(root().join(path), dest).unwrap();
        }
        match mutation {
            "rpm" => edit(f.path(), "packaging/rpm/pgokf.spec", "0.5.0", "0.5.1"),
            "install" => {
                std::fs::remove_file(f.path().join("crates/extension/sql/pgokf--0.5.0.sql"))
                    .unwrap();
            }
            _ => {
                let p = f.path().join("packaging/docker/Dockerfile");
                let mut s = std::fs::read_to_string(&p).unwrap();
                s.push_str("\n# docker run ghcr.io/logicocean/pgokf:0.5.1-pg18\n");
                std::fs::write(p, s).unwrap();
            }
        }
        assert!(
            distribution_identity(f.path(), &source_identity(f.path()).unwrap()).is_err(),
            "{mutation}"
        );
    }
}
