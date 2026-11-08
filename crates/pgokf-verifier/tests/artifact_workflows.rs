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
