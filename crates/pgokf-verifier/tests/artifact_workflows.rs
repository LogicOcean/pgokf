use std::{fs, path::Path};
#[test]
fn selected_source_smoke_uses_current_tooling_and_excludes_nested_checkout() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let workflow: serde_yaml::Value = serde_yaml::from_str(
        &fs::read_to_string(root.join(".github/workflows/packages.yml")).unwrap(),
    )
    .unwrap();
    for (job, command) in [
        ("docker", "image-smoke ."),
        ("companions", "companions-smoke"),
    ] {
        let steps = workflow["jobs"][job]["steps"].as_sequence().unwrap();
        let smoke = steps
            .iter()
            .find(|s| s["name"].as_str() == Some("Smoke test image"))
            .unwrap();
        let run = smoke["run"].as_str().unwrap();
        assert!(
            run.contains("--manifest-path release-tools/Cargo.toml") && run.contains(command),
            "historical source owns {job} smoke verdict"
        );
        let build = steps
            .iter()
            .position(|s| {
                s["uses"]
                    .as_str()
                    .is_some_and(|v| v.starts_with("docker/build-push-action@"))
            })
            .unwrap();
        assert!(
            steps[..build].iter().any(|s| {
                s["run"].as_str().is_some_and(|run| {
                    run.contains("release-tools/") && run.contains(".dockerignore")
                })
            }),
            "nested tooling enters {job} build context"
        );
    }
}

#[test]
fn release_notice_verification_uses_current_provenance_tooling() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let workflow: serde_yaml::Value = serde_yaml::from_str(
        &fs::read_to_string(root.join(".github/workflows/packages.yml")).unwrap(),
    )
    .unwrap();
    let steps = workflow["jobs"]["notices"]["steps"].as_sequence().unwrap();
    let checkouts: Vec<_> = steps
        .iter()
        .filter(|step| {
            step["uses"]
                .as_str()
                .is_some_and(|uses| uses.starts_with("actions/checkout@"))
        })
        .collect();
    assert_eq!(checkouts.len(), 2);
    assert_eq!(
        checkouts[0]["with"]["ref"],
        "${{ needs.prep.outputs.source_ref }}"
    );
    assert_eq!(checkouts[0]["with"]["path"], "release-source");
    assert_eq!(checkouts[1]["with"]["ref"], "${{ github.sha }}");
    assert_eq!(checkouts[1]["with"]["path"], "release-tools");
    let runs = steps
        .iter()
        .filter_map(|step| step["run"].as_str())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(runs.contains("release-tools/scripts/install-cargo-about.sh"));
    assert!(runs.contains("--manifest-path release-tools/Cargo.toml"));
    assert!(runs.contains("notices-verify release-source"));
}

#[test]
fn cargo_about_install_and_notice_verification_are_shared_and_pinned() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let installer = fs::read_to_string(root.join("scripts/install-cargo-about.sh")).unwrap();
    assert!(installer.contains("VERSION='0.9.2'"));
    assert!(
        installer
            .contains("SHA256='9099a59e820c38a68b9d65f300662a567d56562f9a10f6aa4c7e86c17c2566af'")
    );
    assert!(installer.contains(
        "BINARY_SHA256='b06bd6a8bfd726cffb90e3e0588e3e0b1cfbb582bf6a34f4c1c2692ba8f2e7b8'"
    ));
    let verifier = fs::read_to_string(root.join("crates/pgokf-verifier/src/notices.rs")).unwrap();
    assert!(verifier.contains("target_os = \"linux\""));
    assert!(verifier.contains("b06bd6a8bfd726cffb90e3e0588e3e0b1cfbb582bf6a34f4c1c2692ba8f2e7b8"));
    assert!(installer.contains("sha256sum --check --strict"));
    assert!(installer.contains("PATH=/usr/bin:/bin"));
    assert!(installer.contains("unset BASH_ENV ENV CDPATH TAR_OPTIONS GZIP GZIP_OPT"));
    assert!(installer.contains("install -m 0555"));
    assert!(!installer.contains("curl | tar"));
    assert!(!installer.contains("GITHUB_PATH"));

    let ci = fs::read_to_string(root.join(".github/workflows/ci.yml")).unwrap();
    let packages = fs::read_to_string(root.join(".github/workflows/packages.yml")).unwrap();
    assert_eq!(ci.matches("scripts/install-cargo-about.sh").count(), 1);
    assert_eq!(
        packages
            .matches("run: release-tools/scripts/install-cargo-about.sh")
            .count(),
        1
    );
    assert_eq!(
        ci.matches("notices-verify . \"$RUNNER_TEMP/cargo-about\"")
            .count(),
        1
    );
    assert_eq!(
        packages
            .matches("notices-verify release-source \"$RUNNER_TEMP/cargo-about\"")
            .count(),
        1
    );
    for workflow in [&ci, &packages] {
        assert!(!workflow.contains("cargo-about/releases/download"));
        assert!(!workflow.contains("| tar x"));
    }
}
